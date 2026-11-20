//! Anthropic Messages API adapter (API key auth).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use codecraft_protocol::*;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::{http_client, sse::post_sse, Output, Provider, Request, ToolHost};

pub struct Anthropic {
    pub id: String,
    pub base_url: String,
    pub api_key: String,
    client: reqwest::Client,
}

impl Anthropic {
    pub fn new(id: impl Into<String>, base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self { id: id.into(), base_url: base_url.into(), api_key: api_key.into(), client: http_client() }
    }
}

fn cache() -> Value {
    json!({"type": "ephemeral"})
}

pub(crate) fn convert_messages(msgs: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut push = |role: &str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        if let Some(last) = out.last_mut() {
            if last["role"] == role {
                last["content"].as_array_mut().unwrap().extend(blocks);
                return;
            }
        }
        out.push(json!({"role": role, "content": blocks}));
    };
    for m in msgs {
        match m {
            Message::User { content } => push("user", content.iter().filter_map(user_block).collect()),
            Message::Assistant(a) => {
                let blocks: Vec<Value> = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } if !text.is_empty() => Some(json!({"type":"text","text":text})),
                        ContentBlock::Thinking { thinking, signature: Some(sig) } => {
                            Some(json!({"type":"thinking","thinking":thinking,"signature":sig}))
                        }
                        ContentBlock::ToolCall { id, name, arguments } => {
                            Some(json!({"type":"tool_use","id":id,"name":name,"input":arguments}))
                        }
                        _ => None,
                    })
                    .collect();
                push("assistant", blocks)
            }
            Message::ToolResult { tool_call_id, content, is_error, .. } => {
                let inner: Vec<Value> = content.iter().filter_map(user_block).collect();
                push(
                    "user",
                    vec![json!({"type":"tool_result","tool_use_id":tool_call_id,"content":inner,"is_error":is_error})],
                )
            }
        }
    }
    // Cache breakpoint on the last user message.
    if let Some(last_user) = out.iter_mut().rev().find(|m| m["role"] == "user") {
        if let Some(b) = last_user["content"].as_array_mut().and_then(|a| a.last_mut()) {
            b["cache_control"] = cache();
        }
    }
    out
}

fn user_block(b: &ContentBlock) -> Option<Value> {
    match b {
        ContentBlock::Text { text } => Some(json!({"type":"text","text": if text.is_empty() {"(empty)"} else {text}})),
        ContentBlock::Image { media_type, data } => {
            Some(json!({"type":"image","source":{"type":"base64","media_type":media_type,"data":data}}))
        }
        _ => None,
    }
}

fn budget(level: ThinkingLevel) -> Option<u32> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Low => Some(4_000),
        ThinkingLevel::Medium => Some(12_000),
        ThinkingLevel::High => Some(32_000),
    }
}

#[derive(Default)]
struct Block {
    kind: String,
    text: String,
    signature: Option<String>,
    id: String,
    name: String,
    json: String,
}

#[async_trait]
impl Provider for Anthropic {
    fn id(&self) -> &str {
        &self.id
    }

    async fn sample(
        &self,
        req: Request,
        events: UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
        _host: Option<Arc<dyn ToolHost>>,
    ) -> anyhow::Result<Output> {
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters}))
            .collect();
        if let Some(t) = tools.last_mut() {
            t["cache_control"] = cache();
        }
        let mut max_tokens = req.max_tokens;
        let mut body = json!({
            "model": req.model,
            "system": [{"type":"text","text":req.system,"cache_control":cache()}],
            "messages": convert_messages(&req.messages),
            "tools": tools,
            "stream": true,
        });
        if let Some(b) = budget(req.thinking) {
            max_tokens = max_tokens.max(b + 8_000);
            body["thinking"] = json!({"type":"enabled","budget_tokens": b});
        }
        body["max_tokens"] = json!(max_tokens);

        let http = self
            .client
            .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        let mut stream = Box::pin(post_sse(http).await?);

        let mut blocks: BTreeMap<u64, Block> = BTreeMap::new();
        let mut msg = AssistantMessage { model: req.model.clone(), ..Default::default() };
        loop {
            let item = tokio::select! {
                _ = cancel.cancelled() => { msg.stop_reason = StopReason::Aborted; break; }
                item = stream.next() => item,
            };
            let Some(item) = item else { break };
            let (_, data) = item?;
            let v: Value = match serde_json::from_str(&data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match v["type"].as_str().unwrap_or("") {
                "message_start" => {
                    let u = &v["message"]["usage"];
                    msg.usage.input = u["input_tokens"].as_u64().unwrap_or(0);
                    msg.usage.cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                    msg.usage.cache_write = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                }
                "content_block_start" => {
                    let idx = v["index"].as_u64().unwrap_or(0);
                    let cb = &v["content_block"];
                    let kind = cb["type"].as_str().unwrap_or("").to_string();
                    let b = Block {
                        kind: kind.clone(),
                        id: cb["id"].as_str().unwrap_or("").into(),
                        name: cb["name"].as_str().unwrap_or("").into(),
                        ..Default::default()
                    };
                    if kind == "tool_use" {
                        let _ = events.send(StreamEvent::ToolCallStart { id: b.id.clone(), name: b.name.clone() });
                    }
                    blocks.insert(idx, b);
                }
                "content_block_delta" => {
                    let idx = v["index"].as_u64().unwrap_or(0);
                    let Some(b) = blocks.get_mut(&idx) else { continue };
                    let d = &v["delta"];
                    match d["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            let t = d["text"].as_str().unwrap_or("");
                            b.text.push_str(t);
                            let _ = events.send(StreamEvent::TextDelta(t.into()));
                        }
                        "thinking_delta" => {
                            let t = d["thinking"].as_str().unwrap_or("");
                            b.text.push_str(t);
                            let _ = events.send(StreamEvent::ThinkingDelta(t.into()));
                        }
                        "signature_delta" => {
                            b.signature.get_or_insert_with(String::new).push_str(d["signature"].as_str().unwrap_or(""))
                        }
                        "input_json_delta" => {
                            let j = d["partial_json"].as_str().unwrap_or("");
                            b.json.push_str(j);
                            let _ = events.send(StreamEvent::ToolCallDelta { id: b.id.clone(), json: j.into() });
                        }
                        _ => {}
                    }
                }
                "message_delta" => {
                    if let Some(r) = v["delta"]["stop_reason"].as_str() {
                        msg.stop_reason = match r {
                            "max_tokens" => StopReason::Length,
                            "tool_use" => StopReason::ToolUse,
                            _ => StopReason::Stop,
                        };
                    }
                    if let Some(o) = v["usage"]["output_tokens"].as_u64() {
                        msg.usage.output = o;
                    }
                }
                "error" => {
                    anyhow::bail!(crate::ApiError { status: 500, body: v["error"].to_string() });
                }
                _ => {}
            }
        }
        for (_, b) in blocks {
            match b.kind.as_str() {
                "text" => msg.content.push(ContentBlock::Text { text: b.text }),
                "thinking" => msg.content.push(ContentBlock::Thinking { thinking: b.text, signature: b.signature }),
                "redacted_thinking" => {}
                "tool_use" => {
                    let arguments = if b.json.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&b.json).unwrap_or_else(|_| json!({"__invalid_json": b.json}))
                    };
                    msg.content.push(ContentBlock::ToolCall { id: b.id, name: b.name, arguments })
                }
                _ => {}
            }
        }
        Ok(Output::Message(msg))
    }
}
