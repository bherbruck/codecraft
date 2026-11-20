//! Context compaction: summarize older history, keep recent messages.

use std::sync::Arc;

use codecraft_llm::{Output, Provider, Request};
use codecraft_protocol::{ContentBlock, Message, ThinkingLevel};
use tokio_util::sync::CancellationToken;

pub const KEEP_RECENT_TOKENS: u64 = 20_000;
const TAG: &str = "<compaction_summary>";

pub fn summary_message(summary: &str) -> Message {
    Message::user(format!(
        "{TAG}\nEarlier conversation was compacted to save context. Summary of it:\n\n{summary}\n</compaction_summary>"
    ))
}

fn previous_summary(msgs: &[Message]) -> Option<String> {
    match msgs.first() {
        Some(Message::User { content }) => content.iter().find_map(|b| match b {
            ContentBlock::Text { text } if text.starts_with(TAG) => Some(text.clone()),
            _ => None,
        }),
        _ => None,
    }
}

/// Index of the first message to keep, or None if there's nothing worth compacting.
pub fn cut_point(msgs: &[Message], keep_tokens: u64) -> Option<usize> {
    let mut acc = 0;
    let mut cut = msgs.len();
    for (i, m) in msgs.iter().enumerate().rev() {
        acc += m.approx_tokens();
        cut = i;
        if acc >= keep_tokens {
            break;
        }
    }
    // Move forward to a user message so we never split a tool call from its result.
    while cut < msgs.len() && !matches!(msgs[cut], Message::User { .. }) {
        cut += 1;
    }
    // Need at least two messages before the cut to be worth it.
    if cut < 2 || cut >= msgs.len() {
        // Fall back: keep only the last user message onward.
        let last_user = msgs.iter().rposition(|m| matches!(m, Message::User { .. }))?;
        return (last_user >= 2).then_some(last_user);
    }
    Some(cut)
}

fn render(msgs: &[Message]) -> String {
    let mut s = String::new();
    for m in msgs {
        match m {
            Message::User { content } => {
                for b in content {
                    if let ContentBlock::Text { text } = b {
                        s.push_str(&format!("\n[user]\n{text}\n"));
                    }
                }
            }
            Message::Assistant(a) => {
                let t = a.text();
                if !t.is_empty() {
                    s.push_str(&format!("\n[assistant]\n{t}\n"));
                }
                for (_, name, args) in a.tool_calls() {
                    let a = args.to_string();
                    let a: String = a.chars().take(500).collect();
                    s.push_str(&format!("[tool call] {name} {a}\n"));
                }
            }
            Message::ToolResult { tool_name, content, is_error, .. } => {
                for b in content {
                    if let ContentBlock::Text { text } = b {
                        let t: String = text.chars().take(1500).collect();
                        s.push_str(&format!("[tool result{}] {tool_name}\n{t}\n", if *is_error { " ERROR" } else { "" }));
                    }
                }
            }
        }
    }
    s
}

const TEMPLATE: &str = "Write a structured summary of the coding session transcript below so another agent can \
continue the work seamlessly. Use exactly these markdown sections:

## Goal
## Constraints & preferences
## Progress (done / in progress)
## Key decisions
## Files touched (path: what changed)
## Next steps
## Critical context (exact error messages, commands, values that must not be lost)

Be specific and concise. Output only the summary.";

pub async fn summarize(
    provider: &Arc<dyn Provider>,
    model: &str,
    msgs: &[Message],
    cancel: CancellationToken,
) -> anyhow::Result<String> {
    let mut prompt = String::from(TEMPLATE);
    if let Some(prev) = previous_summary(msgs) {
        prompt.push_str("\n\nThe transcript starts with an earlier summary. Update it with the new information rather than starting over.");
        let _ = prev;
    }
    prompt.push_str("\n\n<transcript>\n");
    prompt.push_str(&render(msgs));
    prompt.push_str("\n</transcript>");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let req = Request {
        model: model.to_string(),
        system: "You summarize coding agent sessions.".into(),
        messages: vec![Message::user(prompt)],
        tools: vec![],
        max_tokens: 8_000,
        thinking: ThinkingLevel::Off,
    };
    match provider.sample(req, tx, cancel, None).await? {
        Output::Message(m) => Ok(m.text()),
        Output::Transcript(ms) => Ok(ms
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => Some(a.text()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_protocol::AssistantMessage;

    #[test]
    fn cuts_at_user() {
        let big = "x".repeat(40_000);
        let msgs = vec![
            Message::user("a"),
            Message::Assistant(AssistantMessage { content: vec![ContentBlock::text(big.clone())], ..Default::default() }),
            Message::user("b"),
            Message::Assistant(AssistantMessage { content: vec![ContentBlock::text(big.clone())], ..Default::default() }),
            Message::user("c"),
            Message::Assistant(AssistantMessage { content: vec![ContentBlock::text("ok")], ..Default::default() }),
        ];
        let cut = cut_point(&msgs, 5_000).unwrap();
        assert!(matches!(msgs[cut], Message::User { .. }));
        assert!(cut >= 2);
    }
}
