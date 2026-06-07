//! Local persistence: a single SQLite database holding sessions and their
//! transcript segments. Audio is stored as WAV files alongside the DB; the path
//! is recorded on the session row.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub duration_secs: Option<f64>,
    pub audio_path: String,
    pub segment_count: i64,
}

#[derive(Debug, Serialize, Clone)]
pub struct SegmentRow {
    pub id: i64,
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub language: Option<String>,
    /// "mic" or "system".
    pub source: String,
}

/// Open (creating if needed) the database at `path` and ensure the schema.
pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("open db {}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    init(&conn)?;
    Ok(conn)
}

fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sessions (
            id           TEXT PRIMARY KEY,
            title        TEXT NOT NULL,
            started_at   TEXT NOT NULL,
            ended_at     TEXT,
            duration_secs REAL,
            audio_path   TEXT NOT NULL,
            claude_session_id TEXT
        );
        CREATE TABLE IF NOT EXISTS segments (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            start      REAL NOT NULL,
            end        REAL NOT NULL,
            text       TEXT NOT NULL,
            language   TEXT,
            source     TEXT NOT NULL DEFAULT 'mic',
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_segments_session ON segments(session_id, start);
        CREATE TABLE IF NOT EXISTS chat_messages (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            role       TEXT NOT NULL,
            text       TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_chat_session ON chat_messages(session_id, id);
        "#,
    )?;
    // Migrate older databases that predate later columns.
    let _ = conn.execute(
        "ALTER TABLE segments ADD COLUMN source TEXT NOT NULL DEFAULT 'mic'",
        [],
    );
    let _ = conn.execute("ALTER TABLE sessions ADD COLUMN claude_session_id TEXT", []);
    Ok(())
}

pub fn create_session(
    conn: &Connection,
    id: &str,
    title: &str,
    started_at: &str,
    audio_path: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sessions (id, title, started_at, audio_path) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![id, title, started_at, audio_path],
    )?;
    Ok(())
}

pub fn end_session(conn: &Connection, id: &str, ended_at: &str, duration_secs: f64) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET ended_at = ?2, duration_secs = ?3 WHERE id = ?1",
        rusqlite::params![id, ended_at, duration_secs],
    )?;
    Ok(())
}

pub fn rename_session(conn: &Connection, id: &str, title: &str) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET title = ?2 WHERE id = ?1",
        rusqlite::params![id, title],
    )?;
    Ok(())
}

pub fn delete_session(conn: &Connection, id: &str) -> Result<Option<String>> {
    let audio_path: Option<String> = conn
        .query_row(
            "SELECT audio_path FROM sessions WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .ok();
    conn.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
    Ok(audio_path)
}

pub fn insert_segment(
    conn: &Connection,
    session_id: &str,
    start: f64,
    end: f64,
    text: &str,
    language: Option<&str>,
    source: &str,
    created_at: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO segments (session_id, start, end, text, language, source, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![session_id, start, end, text, language, source, created_at],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_sessions(conn: &Connection) -> Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.title, s.started_at, s.ended_at, s.duration_secs, s.audio_path,
                (SELECT COUNT(*) FROM segments g WHERE g.session_id = s.id) AS cnt
         FROM sessions s
         ORDER BY s.started_at DESC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(SessionRow {
                id: r.get(0)?,
                title: r.get(1)?,
                started_at: r.get(2)?,
                ended_at: r.get(3)?,
                duration_secs: r.get(4)?,
                audio_path: r.get(5)?,
                segment_count: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_segments(conn: &Connection, session_id: &str) -> Result<Vec<SegmentRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, start, end, text, language, source FROM segments
         WHERE session_id = ?1 ORDER BY start ASC, id ASC",
    )?;
    let rows = stmt
        .query_map([session_id], |r| {
            Ok(SegmentRow {
                id: r.get(0)?,
                start: r.get(1)?,
                end: r.get(2)?,
                text: r.get(3)?,
                language: r.get(4)?,
                source: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ---- Chat (Claude Code) ----

#[derive(Debug, Serialize, Clone)]
pub struct ChatRow {
    pub role: String,
    pub text: String,
    pub created_at: String,
}

pub fn insert_chat(
    conn: &Connection,
    session_id: &str,
    role: &str,
    text: &str,
    created_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO chat_messages (session_id, role, text, created_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![session_id, role, text, created_at],
    )?;
    Ok(())
}

pub fn get_chat(conn: &Connection, session_id: &str) -> Result<Vec<ChatRow>> {
    let mut stmt = conn.prepare(
        "SELECT role, text, created_at FROM chat_messages WHERE session_id = ?1 ORDER BY id ASC",
    )?;
    let rows = stmt
        .query_map([session_id], |r| {
            Ok(ChatRow {
                role: r.get(0)?,
                text: r.get(1)?,
                created_at: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn set_claude_session(conn: &Connection, session_id: &str, claude_sid: &str) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET claude_session_id = ?2 WHERE id = ?1",
        rusqlite::params![session_id, claude_sid],
    )?;
    Ok(())
}

pub fn get_claude_session(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    let sid: Option<String> = conn
        .query_row(
            "SELECT claude_session_id FROM sessions WHERE id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    Ok(sid)
}
