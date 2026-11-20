//! OpenAI Chat Completions adapter. Works for OpenAI and any compatible
//! endpoint (Ollama, OpenRouter, Groq, LM Studio, vLLM...).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use codecraft_protocol::*;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::{http_client, sse::post_sse, Output, Provider, Request, ToolHost};

pub struct OpenAiChat {
    pub id: String,
    pub base_url: String,
    pub api_key: Option<String>,
    client: reqwest::Client,
}

impl OpenAiChat {
    pub fn new(id: impl Into<String>, base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self { id: id.into(), base_url: base_url.into(), api_key, client: http_client() }
    }
}

fn convert(system: &str, msgs: &[Message]) -> Vec<Value> {
    let mut out = vec![json!({"role":"system","content":system})];
    for m in msgs {
        match m {
            Message::User { content } => {
                let parts: Vec<Value> = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(json!({"type":"text","text":text})),
                        ContentBlock::Image { media_type, data } => Some(
                            json!({"type":"image_url","image_url":{"url":format!("data:{media_type};base64,{data}")}}),
                        ),
                        _ => None,
                    })
                    .collect();
                // Plain string when text-only: widest compatibility.
                if parts.iter().all(|p| p["type"] == "text") {
                    let s: Vec<&str> = parts.iter().filter_map(|p| p["text"].as_str()).collect();
                    out.push(json!({"role":"user","content":s.join("\n")}));
                } else {
                    out.push(json!({"role":"user","content":parts}));
                }
            }
            Message::Assistant(a) => {
                let text = a.text();
                let calls: Vec<Value> = a
                    .tool_calls()
                    .map(|(id, name, args)| {
                        json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})
                    })
                    .collect();
                let mut v = json!({"role":"assistant","content": if text.is_empty() { Value::Null } else { json!(text) }});
                if !calls.is_empty() {
                    v["tool_calls"] = json!(calls);
                }
                out.push(v);
            }
            Message::ToolResult { tool_call_id, content, .. } => {
                let text: Vec<&str> = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                out.push(json!({"role":"tool","tool_call_id":tool_call_id,"content":text.join("\n")}));
            }
        }
    }
    out
}

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    args: String,
}

#[async_trait]
impl Provider for OpenAiChat {
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
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.parameters}}))
            .collect();
        let mut body = json!({
            "model": req.model,
            "messages": convert(&req.system, &req.messages),
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_completion_tokens": req.max_tokens,
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        match req.thinking {
            ThinkingLevel::Off => {}
            l => body["reasoning_effort"] = json!(format!("{l:?}").to_lowercase()),
        }
        let mut http = self.client.post(format!("{}/chat/completions", self.base_url.trim_end_matches('/'))).json(&body);
        if let Some(k) = &self.api_key {
            http = http.bearer_auth(k);
        }
        let mut stream = Box::pin(post_sse(http).await?);

        let mut msg = AssistantMessage { model: req.model.clone(), ..Default::default() };
        let mut text = String::new();
        let mut thinking = String::new();
        let mut calls: BTreeMap<u64, Call> = BTreeMap::new();
        loop {
            let item = tokio::select! {
                _ = cancel.cancelled() => { msg.stop_reason = StopReason::Aborted; break; }
                item = stream.next() => item,
            };
            let Some(item) = item else { break };
            let (_, data) = item?;
            if data.trim() == "[DONE]" {
                break;
            }
            let v: Value = match serde_json::from_str(&data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(err) = v.get("error") {
                anyhow::bail!(crate::ApiError { status: 500, body: err.to_string() });
            }
            if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
                msg.usage.input = u["prompt_tokens"].as_u64().unwrap_or(0).saturating_sub(cached);
                msg.usage.cache_read = cached;
                msg.usage.output = u["completion_tokens"].as_u64().unwrap_or(0);
            }
            let Some(choice) = v["choices"].get(0) else { continue };
            let d = &choice["delta"];
            if let Some(t) = d["content"].as_str() {
                if !t.is_empty() {
                    text.push_str(t);
                    let _ = events.send(StreamEvent::TextDelta(t.into()));
                }
            }
            for key in ["reasoning_content", "reasoning"] {
                if let Some(t) = d[key].as_str() {
                    if !t.is_empty() {
                        thinking.push_str(t);
                        let _ = events.send(StreamEvent::ThinkingDelta(t.into()));
                    }
                }
            }
            if let Some(tcs) = d["tool_calls"].as_array() {
                for (n, tc) in tcs.iter().enumerate() {
                    let idx = tc["index"].as_u64().unwrap_or(n as u64);
                    let c = calls.entry(idx).or_default();
                    if let Some(id) = tc["id"].as_str() {
                        if c.id.is_empty() {
                            c.id = id.into();
                        }
                    }
                    if let Some(name) = tc["function"]["name"].as_str() {
                        if c.name.is_empty() {
                            c.name = name.into();
                            let _ = events.send(StreamEvent::ToolCallStart { id: c.id.clone(), name: c.name.clone() });
                        }
                    }
                    if let Some(a) = tc["function"]["arguments"].as_str() {
                        c.args.push_str(a);
                        let _ = events.send(StreamEvent::ToolCallDelta { id: c.id.clone(), json: a.into() });
                    }
                }
            }
            if let Some(r) = choice["finish_reason"].as_str() {
                msg.stop_reason = match r {
                    "length" => StopReason::Length,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    _ => StopReason::Stop,
                };
            }
        }
        if !thinking.is_empty() {
            msg.content.push(ContentBlock::Thinking { thinking, signature: None });
        }
        if !text.is_empty() {
            msg.content.push(ContentBlock::Text { text });
        }
        for (_, c) in calls {
            let arguments = if c.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&c.args).unwrap_or_else(|_| json!({"__invalid_json": c.args}))
            };
            let id = if c.id.is_empty() { format!("call_{}", uuid::Uuid::new_v4().simple()) } else { c.id };
            msg.content.push(ContentBlock::ToolCall { id, name: c.name, arguments });
        }
        if msg.stop_reason != StopReason::Aborted && msg.tool_calls().next().is_some() {
            msg.stop_reason = if msg.stop_reason == StopReason::Length { StopReason::Length } else { StopReason::ToolUse };
        }
        Ok(Output::Message(msg))
    }
}
