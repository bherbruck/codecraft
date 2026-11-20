//! Multi-line input buffer with a cursor and history.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct Composer {
    text: String,
    /// Byte offset.
    cursor: usize,
    history: Vec<String>,
    hist_idx: Option<usize>,
}

impl Composer {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }
    pub fn take(&mut self) -> String {
        let t = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.hist_idx = None;
        if !t.trim().is_empty() && self.history.last() != Some(&t) {
            self.history.push(t.clone());
        }
        t
    }
    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }
    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }
    pub fn line_count(&self) -> usize {
        self.text.split('\n').count().max(1)
    }
    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor].char_indices().last().map(|(i, _)| i).unwrap_or(0)
    }
    fn next_boundary(&self) -> usize {
        self.text[self.cursor..].chars().next().map(|c| self.cursor + c.len_utf8()).unwrap_or(self.cursor)
    }
    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }
    fn line_end(&self) -> usize {
        self.text[self.cursor..].find('\n').map(|i| self.cursor + i).unwrap_or(self.text.len())
    }

    pub fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('a') if ctrl => self.cursor = self.line_start(),
            KeyCode::Char('e') if ctrl => self.cursor = self.line_end(),
            KeyCode::Char('u') if ctrl => {
                let s = self.line_start();
                self.text.replace_range(s..self.cursor, "");
                self.cursor = s;
            }
            KeyCode::Char('k') if ctrl => {
                let e = self.line_end();
                self.text.replace_range(self.cursor..e, "");
            }
            KeyCode::Char('w') if ctrl => {
                let before = &self.text[..self.cursor];
                let trimmed = before.trim_end();
                let start = trimmed.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Char(c) if !ctrl => self.insert(c),
            KeyCode::Backspace if self.cursor > 0 => {
                let p = self.prev_boundary();
                self.text.replace_range(p..self.cursor, "");
                self.cursor = p;
            }
            KeyCode::Delete if self.cursor < self.text.len() => {
                let n = self.next_boundary();
                self.text.replace_range(self.cursor..n, "");
            }
            KeyCode::Left => self.cursor = self.prev_boundary(),
            KeyCode::Right => self.cursor = self.next_boundary(),
            KeyCode::Home => self.cursor = self.line_start(),
            KeyCode::End => self.cursor = self.line_end(),
            KeyCode::Up => {
                if self.text.contains('\n') && self.line_start() > 0 {
                    let col = self.cursor - self.line_start();
                    let prev_end = self.line_start() - 1;
                    let prev_start = self.text[..prev_end].rfind('\n').map(|i| i + 1).unwrap_or(0);
                    self.cursor = clamp_boundary(&self.text, (prev_start + col).min(prev_end));
                } else if !self.history.is_empty() {
                    let i = match self.hist_idx {
                        None => self.history.len() - 1,
                        Some(i) => i.saturating_sub(1),
                    };
                    self.hist_idx = Some(i);
                    self.text = self.history[i].clone();
                    self.cursor = self.text.len();
                }
            }
            KeyCode::Down => {
                if self.text.contains('\n') && self.line_end() < self.text.len() {
                    let col = self.cursor - self.line_start();
                    let next_start = self.line_end() + 1;
                    let next_end = self.text[next_start..].find('\n').map(|i| next_start + i).unwrap_or(self.text.len());
                    self.cursor = clamp_boundary(&self.text, (next_start + col).min(next_end));
                } else if let Some(i) = self.hist_idx {
                    if i + 1 < self.history.len() {
                        self.hist_idx = Some(i + 1);
                        self.text = self.history[i + 1].clone();
                    } else {
                        self.hist_idx = None;
                        self.text.clear();
                    }
                    self.cursor = self.text.len();
                }
            }
            _ => {}
        }
    }

    /// Visible lines (hard-wrapped to width, scrolled to the cursor) and the cursor (col, row).
    pub fn view(&self, width: u16, height: u16) -> (Vec<String>, (u16, u16)) {
        let width = width.max(4) as usize;
        let mut rows: Vec<String> = vec![String::new()];
        let mut col = 0usize;
        let mut cur = (0u16, 0u16);
        for (i, c) in self.text.char_indices() {
            if i == self.cursor {
                cur = (col as u16, (rows.len() - 1) as u16);
            }
            if c == '\n' {
                rows.push(String::new());
                col = 0;
                continue;
            }
            let w = c.width().unwrap_or(0);
            if col + w > width {
                rows.push(String::new());
                col = 0;
            }
            rows.last_mut().unwrap().push(c);
            col += w;
        }
        if self.cursor >= self.text.len() {
            if col >= width {
                rows.push(String::new());
                col = 0;
            }
            cur = (col as u16, (rows.len() - 1) as u16);
        }
        let h = height.max(1) as usize;
        let start = (cur.1 as usize + 1).saturating_sub(h);
        let visible = rows.into_iter().skip(start).take(h).collect();
        (visible, (cur.0, cur.1 - start as u16))
    }
}

fn clamp_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
