//! Multi-edit tool: every edit is matched against the original file, must be
//! unique and non-overlapping. Exact match first, then a whitespace/unicode
//! tolerant match. BOM and CRLF line endings are preserved.

use async_trait::async_trait;
use codecraft_protocol::ToolSpec;
use serde_json::{json, Value};
use unicode_normalization::UnicodeNormalization;

use crate::{arg_str, Access, Tool, ToolCtx, ToolOutput};

pub struct Edit;

#[derive(Debug)]
struct Pair {
    old: String,
    new: String,
}

fn parse_edits(args: &Value) -> Result<Vec<Pair>, String> {
    let get = |v: &Value, a: &str, b: &str| -> Option<String> {
        v.get(a).or_else(|| v.get(b)).and_then(|x| x.as_str()).map(String::from)
    };
    if let Some(arr) = args.get("edits").and_then(|e| e.as_array()) {
        let mut out = Vec::new();
        for (i, e) in arr.iter().enumerate() {
            let old = get(e, "old_text", "oldText").ok_or(format!("edits[{i}] is missing old_text"))?;
            let new = get(e, "new_text", "newText").ok_or(format!("edits[{i}] is missing new_text"))?;
            out.push(Pair { old, new });
        }
        if out.is_empty() {
            return Err("`edits` is empty".into());
        }
        return Ok(out);
    }
    match (get(args, "old_text", "oldText"), get(args, "new_text", "newText")) {
        (Some(old), Some(new)) => Ok(vec![Pair { old, new }]),
        _ => Err("provide `edits: [{old_text, new_text}]`".into()),
    }
}

fn fold_char(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        c => c,
    }
}

/// Normalized chars with the original byte range each came from.
fn normalize(s: &str) -> Vec<(char, usize, usize)> {
    let mut out: Vec<(char, usize, usize)> = Vec::new();
    for (i, c) in s.char_indices() {
        let end = i + c.len_utf8();
        for n in std::iter::once(c).nfkc() {
            out.push((fold_char(n), i, end));
        }
    }
    // Drop trailing whitespace on each line.
    let mut res = Vec::with_capacity(out.len());
    let mut pending: Vec<(char, usize, usize)> = Vec::new();
    for x in out {
        if x.0 == '\n' {
            pending.clear();
            res.push(x);
        } else if x.0 == ' ' || x.0 == '\t' {
            pending.push(x);
        } else {
            res.append(&mut pending);
            res.push(x);
        }
    }
    res
}

fn find_all<T: PartialEq>(hay: &[T], needle: &[T]) -> Vec<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return vec![];
    }
    (0..=hay.len() - needle.len()).filter(|&i| hay[i..i + needle.len()] == *needle).collect()
}

/// Find the unique byte range of `old` in `content`.
fn locate(content: &str, old: &str) -> Result<(usize, usize), String> {
    let exact: Vec<usize> = content.match_indices(old).map(|(i, _)| i).collect();
    match exact.len() {
        1 => return Ok((exact[0], exact[0] + old.len())),
        n if n > 1 => {
            return Err(format!("old_text matches {n} times; add surrounding context to make it unique"))
        }
        _ => {}
    }
    let hay = normalize(content);
    let hay_chars: Vec<char> = hay.iter().map(|x| x.0).collect();
    let needle: Vec<char> = normalize(old).iter().map(|x| x.0).collect();
    // Trailing whitespace of the needle itself is insignificant.
    let mut needle = needle;
    while needle.last().map(|c| c.is_whitespace() && *c != '\n').unwrap_or(false) {
        needle.pop();
    }
    if needle.is_empty() {
        return Err("old_text is empty".into());
    }
    let hits = find_all(&hay_chars, &needle);
    match hits.len() {
        1 => {
            let s = hay[hits[0]].1;
            let e = hay[hits[0] + needle.len() - 1].2;
            Ok((s, e))
        }
        0 => Err("old_text not found (also tried ignoring trailing whitespace, smart quotes and unicode dashes). \
                  Re-read the file and copy the text exactly."
            .into()),
        n => Err(format!("old_text matches {n} times (fuzzy); add surrounding context to make it unique")),
    }
}

pub fn apply_edits(original: &str, edits: &[(String, String)]) -> Result<String, String> {
    let mut ranges: Vec<(usize, usize, usize)> = Vec::new();
    for (i, (old, _)) in edits.iter().enumerate() {
        let (s, e) = locate(original, old).map_err(|m| format!("edit {}: {m}", i + 1))?;
        ranges.push((s, e, i));
    }
    ranges.sort();
    for w in ranges.windows(2) {
        if w[1].0 < w[0].1 {
            return Err(format!("edits {} and {} overlap", w[0].2 + 1, w[1].2 + 1));
        }
    }
    let mut out = original.to_string();
    for (s, e, i) in ranges.into_iter().rev() {
        out.replace_range(s..e, &edits[i].1);
    }
    Ok(out)
}

fn short_diff(a: &str, b: &str, path: &str) -> String {
    let diff = similar::TextDiff::from_lines(a, b);
    let s = diff.unified_diff().context_radius(2).header(path, path).to_string();
    if s.len() > 6000 {
        let mut end = 6000;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n... [diff truncated]", &s[..end])
    } else {
        s
    }
}

#[async_trait]
impl Tool for Edit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: "Edit a file by exact text replacement. Each `old_text` must match exactly one place in \
                          the ORIGINAL file (include enough context to be unique). Multiple edits are applied \
                          together and must not overlap. Read the file first."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {"type": "string", "description": "Exact text to replace"},
                                "new_text": {"type": "string", "description": "Replacement text"}
                            },
                            "required": ["old_text", "new_text"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }),
        }
    }
    fn access(&self, args: &Value, ctx: &ToolCtx) -> Access {
        let p = ctx.resolve(args["path"].as_str().unwrap_or(""));
        Access::Write { summary: format!("edit {}", p.display()), key: "write".into() }
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let path = match arg_str(&args, "path") {
            Ok(p) => ctx.resolve(p),
            Err(e) => return ToolOutput::err(e),
        };
        let pairs = match parse_edits(&args) {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(e),
        };
        let _g = ctx.file_locks.lock(&path).await;
        let raw = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
        };
        let (bom, body) = match raw.strip_prefix('\u{FEFF}') {
            Some(b) => (true, b),
            None => (false, raw.as_str()),
        };
        let crlf = body.contains("\r\n");
        let lf = body.replace("\r\n", "\n");
        let edits: Vec<(String, String)> =
            pairs.into_iter().map(|p| (p.old.replace("\r\n", "\n"), p.new.replace("\r\n", "\n"))).collect();
        let updated = match apply_edits(&lf, &edits) {
            Ok(u) => u,
            Err(e) => return ToolOutput::err(format!("{}: {e}", path.display())),
        };
        if updated == lf {
            return ToolOutput::err("edits produced no change");
        }
        let mut final_text = if crlf { updated.replace('\n', "\r\n") } else { updated.clone() };
        if bom {
            final_text.insert(0, '\u{FEFF}');
        }
        if let Err(e) = tokio::fs::write(&path, final_text).await {
            return ToolOutput::err(format!("cannot write {}: {e}", path.display()));
        }
        ToolOutput::ok(format!(
            "Applied {} edit(s) to {}\n{}",
            edits.len(),
            path.display(),
            short_diff(&lf, &updated, &path.display().to_string())
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_fuzzy() {
        let src = "fn a() {\n    let x = “hi”;   \n}\n";
        let out = apply_edits(src, &[("let x = \"hi\";\n".into(), "let x = 1;\n".into())]).unwrap();
        assert_eq!(out, "fn a() {\n    let x = 1;\n}\n");
    }

    #[test]
    fn ambiguous() {
        assert!(apply_edits("a\na\n", &[("a".into(), "b".into())]).is_err());
    }

    #[test]
    fn multi_non_overlapping() {
        let out = apply_edits("one two three", &[("one".into(), "1".into()), ("three".into(), "3".into())]).unwrap();
        assert_eq!(out, "1 two 3");
        assert!(apply_edits("one two", &[("one two".into(), "x".into()), ("two".into(), "y".into())]).is_err());
    }
}
