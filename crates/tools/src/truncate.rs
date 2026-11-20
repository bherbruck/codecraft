//! Output truncation by lines and bytes.

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

pub struct Truncated {
    pub text: String,
    pub truncated: bool,
    pub total_lines: usize,
    /// For head truncation: number of lines kept.
    pub kept_lines: usize,
}

/// Keep the beginning.
pub fn head(s: &str, max_lines: usize, max_bytes: usize) -> Truncated {
    let total_lines = s.lines().count();
    let mut out = String::new();
    let mut kept = 0;
    for line in s.lines() {
        if kept >= max_lines || out.len() + line.len() + 1 > max_bytes {
            return Truncated { text: out, truncated: true, total_lines, kept_lines: kept };
        }
        out.push_str(line);
        out.push('\n');
        kept += 1;
    }
    Truncated { text: out, truncated: false, total_lines, kept_lines: kept }
}

/// Keep the head and tail, drop the middle (for command output).
pub fn head_tail(s: &str, max_lines: usize, max_bytes: usize) -> Truncated {
    let lines: Vec<&str> = s.lines().collect();
    let total_lines = lines.len();
    if total_lines <= max_lines && s.len() <= max_bytes {
        return Truncated { text: s.to_string(), truncated: false, total_lines, kept_lines: total_lines };
    }
    let half_l = max_lines / 2;
    let half_b = max_bytes / 2;
    let mut head = String::new();
    let mut i = 0;
    while i < lines.len() && i < half_l && head.len() + lines[i].len() < half_b {
        head.push_str(lines[i]);
        head.push('\n');
        i += 1;
    }
    let mut tail: Vec<&str> = Vec::new();
    let mut tb = 0;
    let mut j = lines.len();
    while j > i && tail.len() < half_l && tb + lines[j - 1].len() < half_b {
        j -= 1;
        tb += lines[j].len() + 1;
        tail.push(lines[j]);
    }
    tail.reverse();
    let omitted = j - i;
    let text = format!("{head}\n... [{omitted} lines omitted] ...\n\n{}", tail.join("\n"));
    Truncated { text, truncated: true, total_lines, kept_lines: i + tail.len() }
}

pub fn clip_line(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [line truncated]", &s[..end])
}
