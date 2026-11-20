//! Turning text into styled, wrapped lines for the scrollback.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

pub fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// One-line summary of tool arguments.
pub fn summarize_args(name: &str, args: &Value) -> String {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let out = match name {
        "read" | "write" | "ls" => {
            let mut p = s("path");
            if let Some(o) = args.get("offset").and_then(|v| v.as_u64()) {
                p.push_str(&format!(":{o}"));
            }
            p
        }
        "edit" => {
            let n = args.get("edits").and_then(|e| e.as_array()).map(|a| a.len()).unwrap_or(1);
            format!("{} ({n} edit{})", s("path"), if n == 1 { "" } else { "s" })
        }
        "bash" => s("command"),
        "grep" => {
            let mut x = format!("/{}/", s("pattern"));
            if !s("path").is_empty() {
                x.push_str(&format!(" in {}", s("path")));
            }
            if !s("glob").is_empty() {
                x.push_str(&format!(" ({})", s("glob")));
            }
            x
        }
        "glob" => s("pattern"),
        "task" => format!("{}: {}", s("agent"), s("description")),
        _ => args.to_string(),
    };
    let mut out = out;
    if let Ok(cwd) = std::env::current_dir() {
        let prefix = format!("{}/", cwd.display());
        out = out.replace(&prefix, "");
    }
    let one: String = out.replace('\n', " ⏎ ");
    if one.chars().count() > 120 {
        format!("{}…", one.chars().take(119).collect::<String>())
    } else {
        one
    }
}

/// Minimal inline markdown: `code`, **bold**.
fn inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut cur = String::new();
    let mut code = false;
    let mut bold = false;
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let style = |code: bool, bold: bool| {
        let mut s = base;
        if code {
            s = s.fg(Color::Yellow);
        }
        if bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        s
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            if !cur.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut cur), style(code, bold)));
            }
            code = !code;
        } else if !code && c == '*' && chars.get(i + 1) == Some(&'*') {
            if !cur.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut cur), style(code, bold)));
            }
            bold = !bold;
            i += 1;
        } else {
            cur.push(c);
        }
        i += 1;
    }
    if !cur.is_empty() {
        spans.push(Span::styled(cur, style(code, bold)));
    }
    spans
}

/// Markdown renderer state carried across streamed lines.
#[derive(Default)]
pub struct Md {
    pub in_code: bool,
}

impl Md {
    /// Render one complete source line into wrapped display lines.
    pub fn line(&mut self, src: &str, width: u16) -> Vec<Line<'static>> {
        let w = (width as usize).max(10);
        let trimmed = src.trim_start();
        if trimmed.starts_with("```") {
            self.in_code = !self.in_code;
            let lang = trimmed.trim_start_matches('`');
            return vec![Line::from(Span::styled(
                if self.in_code { format!("┌ {lang}") } else { "└".to_string() },
                dim(),
            ))];
        }
        if self.in_code {
            return wrap(src, w.saturating_sub(2))
                .into_iter()
                .map(|l| Line::from(vec![Span::styled("│ ", dim()), Span::styled(l, Style::default().fg(Color::Cyan))]))
                .collect();
        }
        if let Some(h) = trimmed.strip_prefix('#') {
            let text = h.trim_start_matches('#').trim();
            return wrap(text, w)
                .into_iter()
                .map(|l| Line::from(Span::styled(l, Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED))))
                .collect();
        }
        let indent = src.len() - trimmed.len();
        let (bullet, rest) = if let Some(r) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            ("• ".to_string(), r)
        } else if let Some(pos) = trimmed.find(". ").filter(|&p| p <= 3 && trimmed[..p].chars().all(|c| c.is_ascii_digit())) {
            (trimmed[..pos + 2].to_string(), &trimmed[pos + 2..])
        } else {
            (String::new(), trimmed)
        };
        let lead = " ".repeat(indent) + &bullet;
        let lead_w = unicode_width::UnicodeWidthStr::width(lead.as_str());
        let mut out = Vec::new();
        for (i, l) in wrap(rest, w.saturating_sub(lead_w).max(10)).into_iter().enumerate() {
            let mut spans = vec![Span::raw(if i == 0 { lead.clone() } else { " ".repeat(lead_w) })];
            spans.extend(inline(&l, Style::default()));
            out.push(Line::from(spans));
        }
        if out.is_empty() {
            out.push(Line::from(""));
        }
        out
    }
}

pub fn wrap(s: &str, width: usize) -> Vec<String> {
    if s.is_empty() {
        return vec![String::new()];
    }
    textwrap::wrap(s, textwrap::Options::new(width.max(1)).break_words(true))
        .into_iter()
        .map(|c| c.into_owned())
        .collect()
}

/// Plain wrapped lines in a single style, with a prefix on the first line.
pub fn styled_block(prefix: &str, text: &str, style: Style, width: u16) -> Vec<Line<'static>> {
    let pw = unicode_width::UnicodeWidthStr::width(prefix);
    let mut out = Vec::new();
    for (n, src) in text.lines().enumerate() {
        for (i, l) in wrap(src, (width as usize).saturating_sub(pw).max(10)).into_iter().enumerate() {
            let p = if n == 0 && i == 0 { prefix.to_string() } else { " ".repeat(pw) };
            out.push(Line::from(vec![Span::styled(p, style), Span::styled(l, style)]));
        }
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(prefix.to_string(), style)));
    }
    out
}
