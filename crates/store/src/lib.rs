//! SQLite session store. One database: `~/.codecraft/codecraft.db`.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use codecraft_core::persist::{Entry, Persist, SessionMeta};
use codecraft_protocol::{ContentBlock, Message};
use rusqlite::{params, Connection, OptionalExtension};

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub parent_id: Option<String>,
    pub cwd: String,
    pub model: String,
    pub agent: Option<String>,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub entries: i64,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY,
    parent_id   TEXT REFERENCES sessions(id) ON DELETE CASCADE,
    cwd         TEXT NOT NULL,
    model       TEXT NOT NULL,
    agent       TEXT,
    title       TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_cwd ON sessions(cwd, updated_at);
CREATE INDEX IF NOT EXISTS sessions_parent ON sessions(parent_id);
CREATE TABLE IF NOT EXISTS entries (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    id          TEXT NOT NULL UNIQUE,
    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    parent_id   TEXT,
    kind        TEXT NOT NULL,
    data        TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS entries_session ON entries(session_id, seq);
CREATE TABLE IF NOT EXISTS approvals (
    project     TEXT NOT NULL,
    rule        TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (project, rule)
);
CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(text, session_id UNINDEXED, entry_id UNINDEXED);
"#;

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn entry_kind(e: &Entry) -> &'static str {
    match e {
        Entry::Message { .. } => "message",
        Entry::Compaction { .. } => "compaction",
        Entry::ModelChange { .. } => "model_change",
    }
}

fn searchable_text(e: &Entry) -> Option<String> {
    let Entry::Message { message } = e else { return None };
    let blocks = match message {
        Message::User { content } => content,
        Message::Assistant(a) => &a.content,
        Message::ToolResult { .. } => return None,
    };
    let t: Vec<&str> = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    (!t.is_empty()).then(|| t.join("\n"))
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    pub fn open_default() -> anyhow::Result<Store> {
        Self::open(&codecraft_core::config::home_dir().join("codecraft.db"))
    }

    fn row(r: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
        Ok(SessionRow {
            id: r.get(0)?,
            parent_id: r.get(1)?,
            cwd: r.get(2)?,
            model: r.get(3)?,
            agent: r.get(4)?,
            title: r.get(5)?,
            created_at: r.get(6)?,
            updated_at: r.get(7)?,
            entries: r.get(8)?,
        })
    }

    const SELECT: &'static str = "SELECT s.id, s.parent_id, s.cwd, s.model, s.agent, s.title, s.created_at, s.updated_at, \
         (SELECT COUNT(*) FROM entries e WHERE e.session_id = s.id) FROM sessions s";

    /// Top-level sessions (not sub-agents), newest first.
    pub fn list(&self, cwd: Option<&str>, limit: usize) -> anyhow::Result<Vec<SessionRow>> {
        let c = self.conn.lock().unwrap();
        let sql = format!(
            "{} WHERE s.parent_id IS NULL AND (?1 IS NULL OR s.cwd = ?1) ORDER BY s.updated_at DESC LIMIT ?2",
            Self::SELECT
        );
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(params![cwd, limit as i64], Self::row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn children(&self, id: &str) -> anyhow::Result<Vec<SessionRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(&format!("{} WHERE s.parent_id = ?1 ORDER BY s.created_at", Self::SELECT))?;
        let rows = st.query_map(params![id], Self::row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get(&self, id: &str) -> anyhow::Result<Option<SessionRow>> {
        let c = self.conn.lock().unwrap();
        // Allow unique id prefixes.
        let mut st = c.prepare(&format!("{} WHERE s.id LIKE ?1 || '%' ORDER BY s.updated_at DESC LIMIT 2", Self::SELECT))?;
        let rows = st.query_map(params![id], Self::row)?.collect::<Result<Vec<_>, _>>()?;
        if rows.len() > 1 && rows[0].id != id {
            anyhow::bail!("session id prefix `{id}` is ambiguous");
        }
        Ok(rows.into_iter().next())
    }

    pub fn entries(&self, id: &str) -> anyhow::Result<Vec<Entry>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT data FROM entries WHERE session_id = ?1 ORDER BY seq")?;
        let rows = st.query_map(params![id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// Search user/assistant text across sessions.
    pub fn search(&self, q: &str, limit: usize) -> anyhow::Result<Vec<(SessionRow, String)>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT session_id, snippet(entries_fts, 0, '[', ']', '…', 12) FROM entries_fts \
             WHERE entries_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )?;
        let hits = st
            .query_map(params![q, limit as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(st);
        drop(c);
        let mut out = Vec::new();
        for (sid, snip) in hits {
            if let Some(row) = self.get(&sid)? {
                out.push((row, snip));
            }
        }
        Ok(out)
    }

    pub fn latest(&self, cwd: &str) -> anyhow::Result<Option<SessionRow>> {
        Ok(self.list(Some(cwd), 1)?.into_iter().next())
    }

    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM entries_fts WHERE session_id = ?1", params![id])?;
        c.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// JSONL export: a header line then one entry per line.
    pub fn export_jsonl(&self, id: &str) -> anyhow::Result<String> {
        let row = self.get(id)?.ok_or_else(|| anyhow::anyhow!("no session {id}"))?;
        let mut out = serde_json::to_string(&serde_json::json!({
            "type": "session", "id": row.id, "parent_id": row.parent_id, "cwd": row.cwd,
            "model": row.model, "agent": row.agent, "title": row.title, "created_at": row.created_at,
        }))?;
        out.push('\n');
        for e in self.entries(&row.id)? {
            out.push_str(&serde_json::to_string(&e)?);
            out.push('\n');
        }
        Ok(out)
    }

    pub fn saved_rules(&self, project: &str) -> anyhow::Result<Vec<String>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT rule FROM approvals WHERE project = ?1")?;
        let rows = st.query_map(params![project], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    }

    pub fn save_rule(&self, project: &str, rule: &str) -> anyhow::Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("INSERT OR IGNORE INTO approvals VALUES (?1, ?2, ?3)", params![project, rule, now()])?;
        Ok(())
    }
}

impl Persist for Store {
    fn create_session(&self, m: &SessionMeta) -> anyhow::Result<()> {
        let c = self.conn.lock().unwrap();
        let t = now();
        c.execute(
            "INSERT OR IGNORE INTO sessions (id, parent_id, cwd, model, agent, title, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![m.id, m.parent_id, m.cwd, m.model, m.agent, m.title, t],
        )?;
        Ok(())
    }

    fn append(&self, session_id: &str, e: &Entry) -> anyhow::Result<()> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let parent: Option<String> = tx
            .query_row(
                "SELECT id FROM entries WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()?;
        let id = uuid::Uuid::now_v7().to_string();
        let t = now();
        tx.execute(
            "INSERT INTO entries (id, session_id, parent_id, kind, data, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, session_id, parent, entry_kind(e), serde_json::to_string(e)?, t],
        )?;
        if let Some(text) = searchable_text(e) {
            tx.execute("INSERT INTO entries_fts (text, session_id, entry_id) VALUES (?1, ?2, ?3)", params![text, session_id, id])?;
        }
        tx.execute("UPDATE sessions SET updated_at = ?2 WHERE id = ?1", params![session_id, t])?;
        if let Entry::ModelChange { model } = e {
            tx.execute("UPDATE sessions SET model = ?2 WHERE id = ?1", params![session_id, model])?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("t.db")).unwrap();
        let meta = SessionMeta {
            id: "abc".into(), parent_id: None, cwd: "/x".into(), model: "m/m".into(), agent: None, title: Some("hi".into()),
        };
        s.create_session(&meta).unwrap();
        s.append("abc", &Entry::Message { message: Message::user("hello world") }).unwrap();
        s.append("abc", &Entry::Compaction { summary: "sum".into(), tokens_before: 10 }).unwrap();
        s.append("abc", &Entry::Message { message: Message::user("after") }).unwrap();
        let rebuilt = codecraft_core::persist::rebuild(&s.entries("abc").unwrap());
        assert_eq!(rebuilt.len(), 2);
        assert_eq!(s.list(Some("/x"), 10).unwrap()[0].entries, 3);
        assert_eq!(s.search("hello", 5).unwrap().len(), 1);
        assert!(s.export_jsonl("ab").unwrap().lines().count() == 4);
    }
}
