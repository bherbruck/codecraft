//! Project instruction files (AGENTS.md / CLAUDE.md).

use std::path::{Path, PathBuf};

pub const MAX_BYTES: usize = 32 * 1024;
const NAMES: &[&str] = &["AGENTS.override.md", "AGENTS.md", "CLAUDE.md"];

/// Nearest ancestor with a `.git` entry.
pub fn project_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

pub struct InstructionFile {
    pub path: PathBuf,
    pub content: String,
}

/// Global file, then each directory from the project root down to cwd.
pub fn load_instructions(cwd: &Path) -> Vec<InstructionFile> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let root = project_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let mut d = Some(cwd);
    while let Some(x) = d {
        dirs.push(x.to_path_buf());
        if x == root {
            break;
        }
        d = x.parent();
    }
    dirs.reverse();
    let mut candidates: Vec<PathBuf> = Vec::new();
    let home = crate::config::home_dir();
    if let Some(p) = NAMES.iter().map(|n| home.join(n)).find(|p| p.is_file()) {
        candidates.push(p);
    }
    for dir in dirs {
        if let Some(p) = NAMES.iter().map(|n| dir.join(n)).find(|p| p.is_file()) {
            candidates.push(p);
        }
    }
    let mut total = 0;
    let mut out = Vec::new();
    for path in candidates {
        let Ok(mut content) = std::fs::read_to_string(&path) else { continue };
        if total + content.len() > MAX_BYTES {
            let mut end = MAX_BYTES.saturating_sub(total);
            while !content.is_char_boundary(end) {
                end -= 1;
            }
            content.truncate(end);
            content.push_str("\n[truncated]");
        }
        total += content.len();
        out.push(InstructionFile { path, content });
        if total >= MAX_BYTES {
            break;
        }
    }
    out
}
