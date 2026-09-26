//! SQLite session backend (experimental, mirrors
//! `packages/session-backends/sqlite-node`): sessions and entries stored in
//! `<sessionDir>/sessions.db` instead of per-session JSONL files. The entry
//! payload is the same SessionLine JSON, so files remain byte-compatible in
//! spirit — JSONL stays the default everywhere.

use std::path::{Path, PathBuf};

use crate::entry::{SessionHeader, SessionLine};
use crate::manager::SessionError;

/// One database per session directory (TS sqlite-node layout).
pub fn db_path(session_dir: &Path) -> PathBuf {
    session_dir.join("sessions.db")
}

pub fn open_db(session_dir: &Path) -> Result<rusqlite::Connection, SessionError> {
    std::fs::create_dir_all(session_dir)?;
    let conn = rusqlite::Connection::open(db_path(session_dir))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         -- Wait (instead of erroring with SQLITE_BUSY) when another
         -- connection holds the write lock; without it concurrent appends
         -- from two processes fail spuriously under WAL.
         PRAGMA busy_timeout=5000;
         CREATE TABLE IF NOT EXISTS sessions (
           id TEXT PRIMARY KEY,
           header TEXT NOT NULL,
           created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS entries (
           session_id TEXT NOT NULL,
           seq INTEGER NOT NULL,
           line TEXT NOT NULL,
           PRIMARY KEY (session_id, seq)
         );",
    )?;
    Ok(conn)
}

/// Insert (or replace) the session header row + header entry.
pub fn start_session(
    conn: &rusqlite::Connection,
    header: &SessionHeader,
) -> Result<(), SessionError> {
    let header_json = SessionLine::Header(header.clone()).to_json();
    conn.execute(
        "INSERT OR REPLACE INTO sessions (id, header, created_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![header.id, header_json, header.timestamp],
    )?;
    append_line(conn, &header.id, &SessionLine::Header(header.clone()))
}

/// Append a session line. seq is allocated inside a transaction: the
/// MAX(seq)+1 read and the insert must be atomic, or two writers on the
/// same connection could interleave between them. (Cross-connection
/// duplicates are caught by the PRIMARY KEY instead of silently
/// corrupting the order; busy_timeout makes the loser wait rather than
/// fail with SQLITE_BUSY.)
pub fn append_line(
    conn: &rusqlite::Connection,
    session_id: &str,
    line: &SessionLine,
) -> Result<(), SessionError> {
    // Same at-rest encryption semantics as the JSONL append path: entries
    // encrypt when a session key is installed, and an encryption failure
    // is a hard error — the row is NOT written rather than silently
    // storing plaintext.
    let json = crate::manager::SessionManager::render_append_line(
        line,
        crate::crypto::session_key().is_some(),
        crate::crypto::encrypt_line,
    )?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO entries (session_id, seq, line) VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM entries WHERE session_id = ?1), ?2)",
        rusqlite::params![session_id, json],
    )?;
    tx.commit()?;
    Ok(())
}

/// Load the header + all lines for one session, in order.
pub fn load_session(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<(SessionHeader, Vec<SessionLine>), SessionError> {
    let header_json: String = conn.query_row(
        "SELECT header FROM sessions WHERE id = ?1",
        rusqlite::params![session_id],
        |row| row.get(0),
    )?;
    let line = SessionLine::parse(&header_json)
        .ok_or_else(|| SessionError::MissingHeader(db_path_string(conn).into()))?;
    let SessionLine::Header(header) = line else {
        return Err(SessionError::MissingHeader(db_path_string(conn).into()));
    };

    let mut stmt = conn.prepare("SELECT line FROM entries WHERE session_id = ?1 ORDER BY seq")?;
    let raw_lines = stmt
        .query_map(rusqlite::params![session_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<String>, _>>()?;
    let mut lines = Vec::with_capacity(raw_lines.len());
    for raw in &raw_lines {
        // Mirror SessionManager::open: an undecryptable encrypted row is a
        // hard error, NOT a silently dropped row — opening anyway would
        // drop the encrypted history and keep appending onto a wrong
        // parent chain.
        if crate::crypto::has_undecryptable_encrypted_line(raw) {
            return Err(SessionError::Encrypted(conn_db_path(conn)));
        }
        if let Some(line) = SessionLine::parse(raw) {
            lines.push(line);
        }
    }
    Ok((header, lines))
}

/// Copy a session's rows under a new session id (fork_from for sqlite).
pub fn fork_session(
    conn: &rusqlite::Connection,
    from_id: &str,
    header: &SessionHeader,
) -> Result<(), SessionError> {
    let lines: Vec<String> = {
        let mut stmt =
            conn.prepare("SELECT line FROM entries WHERE session_id = ?1 ORDER BY seq")?;
        stmt.query_map(rusqlite::params![from_id], |row| row.get::<_, String>(0))?
            .filter_map(|l| l.ok())
            // Drop the source session's header line(s): the fork gets the
            // new header, matching the JSONL fork_from (which rewrites the
            // header in place). Keeping it would embed a header with the
            // OLD session id in the fork's entry stream.
            .filter(|l| !matches!(SessionLine::parse(l), Some(SessionLine::Header(_))))
            .collect()
    };
    let header_json = SessionLine::Header(header.clone()).to_json();
    // All-or-nothing: a crash between the header insert and the entry
    // copies must not leave a half-forked session behind.
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT OR REPLACE INTO sessions (id, header, created_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![header.id, header_json, header.timestamp],
    )?;
    tx.execute(
        "INSERT INTO entries (session_id, seq, line) VALUES (?1, 1, ?2)",
        rusqlite::params![header.id, header_json],
    )?;
    for (seq, line) in lines.iter().enumerate() {
        tx.execute(
            "INSERT INTO entries (session_id, seq, line) VALUES (?1, ?2, ?3)",
            rusqlite::params![header.id, (seq + 2) as i64, line],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Session summaries for the session picker (id, header timestamp, cwd).
pub fn list_sessions(
    conn: &rusqlite::Connection,
) -> Result<Vec<(String, SessionHeader)>, SessionError> {
    let mut stmt = conn.prepare("SELECT id, header FROM sessions ORDER BY created_at DESC")?;
    let mut out = Vec::new();
    for row in stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, header_json) = row?;
        if let Some(SessionLine::Header(header)) = SessionLine::parse(&header_json) {
            out.push((id, header));
        }
    }
    Ok(out)
}

fn db_path_string(_conn: &rusqlite::Connection) -> String {
    "sessions.db".to_string()
}

/// Filesystem path of the connection's database (for error reporting).
fn conn_db_path(conn: &rusqlite::Connection) -> PathBuf {
    conn.path()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("sessions.db"))
}

impl From<rusqlite::Error> for SessionError {
    fn from(e: rusqlite::Error) -> Self {
        SessionError::Io(std::io::Error::other(e))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::context::now_iso;
    use crate::entry::SessionHeader;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db(dir.path()).unwrap();
        let header = SessionHeader::new(
            "test-id".to_string(),
            now_iso(),
            "D:/work".to_string(),
            None,
        );
        start_session(&conn, &header).unwrap();
        let (loaded_header, lines) = load_session(&conn, "test-id").unwrap();
        assert_eq!(loaded_header.id, "test-id");
        assert_eq!(lines.len(), 1, "header entry only: {lines:?}");

        let forks = SessionHeader::new(
            "fork-id".to_string(),
            now_iso(),
            "D:/work".to_string(),
            None,
        );
        fork_session(&conn, "test-id", &forks).unwrap();
        let (_, fork_lines) = load_session(&conn, "fork-id").unwrap();
        assert_eq!(fork_lines.len(), 1);

        // The fork's entry stream starts with the NEW header, not the
        // source session's (JSONL fork_from parity).
        let Some(SessionLine::Header(h)) = fork_lines.first() else {
            panic!("header first")
        };
        assert_eq!(h.id, "fork-id");
        assert!(
            fork_lines
                .iter()
                .filter(|l| matches!(l, SessionLine::Header(_)))
                .count()
                == 1
        );

        let sessions = list_sessions(&conn).unwrap();
        assert_eq!(sessions.len(), 2);
    }

    /// Regression: sqlite append used to store line.to_json() verbatim,
    /// so a sessionEncryption-enabled session was persisted in plaintext
    /// with no warning. With a key installed, stored rows must be
    /// encrypted and load back through the decrypting parse path.
    #[test]
    fn append_encrypts_when_key_installed() {
        crate::crypto::install_test_key();
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db(dir.path()).unwrap();
        let header =
            SessionHeader::new("enc-id".to_string(), now_iso(), "D:/work".to_string(), None);
        start_session(&conn, &header).unwrap();

        let entry = SessionLine::parse(
            "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"sqlite secret\",\"timestamp\":1}}",
        )
        .unwrap();
        append_line(&conn, "enc-id", &entry).unwrap();

        // The stored row is ciphertext, not plaintext JSON.
        let raw: String = conn
            .query_row(
                "SELECT line FROM entries WHERE session_id = 'enc-id' AND seq = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(crate::crypto::is_encrypted_line(&raw), "{raw}");
        assert!(!raw.contains("sqlite secret"), "plaintext leaked: {raw}");

        // ... and loads back decrypted.
        let (_, lines) = load_session(&conn, "enc-id").unwrap();
        assert_eq!(lines.len(), 2, "header + entry: {lines:?}");
        assert!(matches!(&lines[1], SessionLine::Entry(e) if e.id() == "m1"));
    }

    /// Regression: load_session used to filter_map(parse), silently
    /// dropping undecryptable encrypted rows — open would then keep
    /// appending onto a wrong parent chain. It must fail loudly with
    /// SessionError::Encrypted, mirroring the JSONL SessionManager::open
    /// guard.
    #[test]
    fn load_session_rejects_undecryptable_lines() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db(dir.path()).unwrap();
        let header = SessionHeader::new(
            "locked-id".to_string(),
            now_iso(),
            "D:/work".to_string(),
            None,
        );
        start_session(&conn, &header).unwrap();
        // Undecryptable regardless of key state (invalid base64 payload):
        // with no key installed this is the "encrypted but no key" case;
        // with a key it's the wrong-key/tampered case.
        conn.execute(
            "INSERT INTO entries (session_id, seq, line) VALUES ('locked-id', 2, 'tack-enc:v1:not-valid-base64!!!')",
            [],
        )
        .unwrap();

        let err = load_session(&conn, "locked-id").unwrap_err();
        assert!(matches!(err, SessionError::Encrypted(_)), "{err:?}");
    }
}
