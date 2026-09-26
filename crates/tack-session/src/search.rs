//! Cross-session full-text search ("how did we solve that last time?").
//! Scans every session under `<agent dir>/sessions/`: per-session JSONL
//! files plus, when a session dir holds a shared `sessions.db`, the SQLite
//! backend. User and assistant message text match case-insensitively.

use std::path::{Path, PathBuf};

use tack_agent_core::AgentMessage;
use tack_ai::{InputContentBlock, UserContent};

use crate::entry::{SessionEntry, SessionHeader, SessionLine};

/// One session with at least one match.
#[derive(Clone, Debug)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub path: PathBuf,
    pub cwd: String,
    pub name: Option<String>,
    pub first_prompt: Option<String>,
    pub timestamp: String,
    /// Total matching messages in the session.
    pub match_count: usize,
    /// Up to `snippets_per_session` (role, snippet) previews.
    pub snippets: Vec<(String, String)>,
    pub modified: std::time::SystemTime,
}

fn message_text(message: &AgentMessage) -> Option<(String, String)> {
    match message {
        AgentMessage::User(u) => {
            let text = match &u.content {
                UserContent::Text(t) => t.clone(),
                UserContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|b| match b {
                        InputContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            Some(("user".to_string(), text))
        }
        AgentMessage::Assistant(a) => Some(("assistant".to_string(), a.text())),
        _ => None,
    }
}

/// ±`radius` chars of context around the first case-insensitive match.
fn snippet(text: &str, query_lower: &str, radius: usize) -> Option<String> {
    // Case-fold for the search, but keep a map from lowercased byte
    // offsets back to byte offsets in `text`: lowercasing can change byte
    // lengths ('İ' → "i\u{307}" grows, 'ẞ' → 'ß' shrinks), so indices
    // into the folded copy are NOT valid indices into the original and
    // would panic or slice the wrong range.
    let mut lower = String::with_capacity(text.len());
    let mut map: Vec<usize> = Vec::with_capacity(text.len() + 1); // lower byte → text byte
    for (tb, ch) in text.char_indices() {
        for lc in ch.to_lowercase() {
            map.extend(std::iter::repeat_n(tb, lc.len_utf8()));
            lower.push(lc);
        }
    }
    map.push(text.len());

    let lb = lower.find(query_lower)?;
    let byte_idx = map[lb];
    let match_end = map[lb + query_lower.len()];

    let start = text[..byte_idx]
        .char_indices()
        .rev()
        .nth(radius)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let end = text[match_end..]
        .char_indices()
        .nth(radius)
        .map(|(i, _)| match_end + i)
        .unwrap_or(text.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(text[start..end].trim());
    if end < text.len() {
        out.push('…');
    }
    Some(out.replace('\n', " "))
}

/// Per-session match accumulator, shared by the JSONL and SQLite scans so
/// both backends get identical match/snippet/first-prompt semantics.
#[derive(Default)]
struct SearchAccum {
    header: Option<SessionHeader>,
    name: Option<String>,
    first_prompt: Option<String>,
    match_count: usize,
    snippets: Vec<(String, String)>,
}

impl SearchAccum {
    fn feed(&mut self, line: &SessionLine, query_lower: &str, snippets_per_session: usize) {
        match line {
            SessionLine::Header(h) => self.header = Some(h.clone()),
            SessionLine::Entry(SessionEntry::SessionInfo { name, .. }) => {
                self.name = name.clone();
            }
            SessionLine::Entry(SessionEntry::Message { message, .. }) => {
                let Some((role, text)) = message_text(message) else {
                    return;
                };
                if role == "user" && self.first_prompt.is_none() {
                    self.first_prompt = Some(
                        text.lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(120)
                            .collect(),
                    );
                }
                if text.to_lowercase().contains(query_lower) {
                    self.match_count += 1;
                    if self.snippets.len() < snippets_per_session
                        && let Some(s) = snippet(&text, query_lower, 80)
                    {
                        self.snippets.push((role, s));
                    }
                }
            }
            _ => {}
        }
    }

    fn into_hit(self, path: PathBuf, modified: std::time::SystemTime) -> Option<SessionSearchHit> {
        if self.match_count == 0 {
            return None;
        }
        let header = self.header?;
        Some(SessionSearchHit {
            session_id: header.id,
            path,
            cwd: header.cwd,
            name: self.name,
            first_prompt: self.first_prompt,
            timestamp: header.timestamp,
            match_count: self.match_count,
            snippets: self.snippets,
            modified,
        })
    }
}

/// Search one session file; None when nothing matches.
fn search_file(
    path: &Path,
    query_lower: &str,
    snippets_per_session: usize,
) -> Option<SessionSearchHit> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut accum = SearchAccum::default();
    // Format v4: fold the transaction log into v3-shaped entries and feed
    // the shared accumulator so both formats get identical semantics.
    if let Some(scan) = crate::v4_bridge::scan_v4_file_content(&content) {
        let header = SessionHeader {
            entry_type: "session".to_string(),
            version: Some(4),
            id: scan.header.id.clone(),
            timestamp: crate::context::millis_to_iso(scan.header.created_at),
            cwd: scan.header.cwd.clone(),
            parent_session: scan.header.parent_session_id.clone(),
        };
        accum.feed(
            &SessionLine::Header(header),
            query_lower,
            snippets_per_session,
        );
        for entry in &scan.entries {
            accum.feed(
                &SessionLine::Entry(entry.clone()),
                query_lower,
                snippets_per_session,
            );
        }
        if let Some(name) = scan.session_name {
            accum.name = Some(name);
        }
        let modified = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        return accum.into_hit(path.to_path_buf(), modified);
    }
    for line in content.lines() {
        if let Some(parsed) = SessionLine::parse(line) {
            accum.feed(&parsed, query_lower, snippets_per_session);
        }
    }
    let modified = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    accum.into_hit(path.to_path_buf(), modified)
}

/// `%`-wrapped LIKE pattern for the SQL pre-filter, or None when the query
/// is not LIKE-safe: SQLite LIKE folds case for ASCII only (the Rust
/// matcher uses full Unicode lowercasing), and it matches against the raw
/// JSON line where `"`, `\` and control chars appear escaped — either gap
/// could produce false negatives, so such queries scan every line in Rust.
fn like_prefilter(query_lower: &str) -> Option<String> {
    if !query_lower.is_ascii()
        || query_lower
            .chars()
            .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        return None;
    }
    let mut pattern = String::with_capacity(query_lower.len() + 2);
    pattern.push('%');
    for c in query_lower.chars() {
        if c == '%' || c == '_' {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    Some(pattern)
}

fn row_string(row: &rusqlite::Row<'_>) -> rusqlite::Result<String> {
    row.get(0)
}

/// Last-write time of the shared db. In WAL mode appends land in
/// `sessions.db-wal` and the main file's mtime only moves on checkpoint,
/// so take the newest of the db files.
fn db_modified(session_dir: &Path) -> std::time::SystemTime {
    let mut latest = std::time::SystemTime::UNIX_EPOCH;
    for suffix in ["sessions.db", "sessions.db-wal", "sessions.db-shm"] {
        if let Ok(m) = std::fs::metadata(session_dir.join(suffix)).and_then(|m| m.modified())
            && m > latest
        {
            latest = m;
        }
    }
    latest
}

/// Search the shared SQLite backend of one session dir (no-op without a
/// `sessions.db`). A SQL-level LIKE pre-filter narrows candidate sessions;
/// every candidate's lines are then re-checked by the same Rust matcher
/// the JSONL scan uses, keeping match/snippet semantics identical.
fn search_sqlite(
    session_dir: &Path,
    query_lower: &str,
    snippets_per_session: usize,
) -> Vec<SessionSearchHit> {
    let db = crate::sqlite_backend::db_path(session_dir);
    if !db.is_file() {
        return Vec::new();
    }
    let Ok(conn) = crate::sqlite_backend::open_db(session_dir) else {
        return Vec::new();
    };
    let pattern = like_prefilter(query_lower);
    // The LIKE pre-filter cannot see inside encrypted rows: with a
    // session key installed, entry rows may be ciphertext, so every
    // session must go through the (decrypting) Rust matcher instead —
    // same fallback as queries SQL LIKE cannot fold.
    let pattern = if crate::crypto::session_key().is_some() {
        None
    } else {
        pattern
    };
    // Newest first, so the stable sort in search_sessions keeps recency
    // order among sqlite hits (they all share the db file's mtime).
    let sql = if pattern.is_some() {
        "SELECT id FROM sessions WHERE id IN (
           SELECT DISTINCT session_id FROM entries WHERE line LIKE ?1 ESCAPE '\\'
         ) ORDER BY created_at DESC"
    } else {
        "SELECT id FROM sessions ORDER BY created_at DESC"
    };
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let ids: Vec<String> = {
        let rows = match &pattern {
            Some(p) => stmt.query_map(rusqlite::params![p], row_string),
            None => stmt.query_map([], row_string),
        };
        let Ok(rows) = rows else {
            return Vec::new();
        };
        rows.flatten().collect()
    };
    drop(stmt);

    let modified = db_modified(session_dir);
    let mut hits = Vec::new();
    for id in ids {
        let Ok((header, lines)) = crate::sqlite_backend::load_session(&conn, &id) else {
            continue;
        };
        let mut accum = SearchAccum::default();
        // The header normally also appears as the first entry line; feeding
        // it explicitly covers dbs where it does not.
        accum.feed(
            &SessionLine::Header(header),
            query_lower,
            snippets_per_session,
        );
        for line in &lines {
            accum.feed(line, query_lower, snippets_per_session);
        }
        if let Some(hit) = accum.into_hit(db.clone(), modified) {
            hits.push(hit);
        }
    }
    hits
}

/// Search all sessions under `<agent dir>/sessions/`, most recent first.
pub fn search_sessions(agent_dir: &Path, query: &str, max_results: usize) -> Vec<SessionSearchHit> {
    let query_lower = query.trim().to_lowercase();
    if query_lower.is_empty() {
        return Vec::new();
    }
    let sessions_root = agent_dir.join("sessions");
    let mut hits = Vec::new();
    if let Ok(dirs) = std::fs::read_dir(&sessions_root) {
        for dir in dirs.flatten() {
            let dir = dir.path();
            if let Ok(files) = std::fs::read_dir(&dir) {
                for file in files.flatten() {
                    let path = file.path();
                    if path.extension().is_none_or(|e| e != "jsonl") {
                        continue;
                    }
                    if let Some(hit) = search_file(&path, &query_lower, 3) {
                        hits.push(hit);
                    }
                }
            }
            hits.extend(search_sqlite(&dir, &query_lower, 3));
        }
    }
    hits.sort_by_key(|h| std::cmp::Reverse(h.modified));
    hits.truncate(max_results);
    hits
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn write_session(dir: &Path, id: &str, lines: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut content = format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-08-28T10:00:00Z\",\"cwd\":\"/work\"}}\n"
        );
        for line in lines {
            content.push_str(line);
            content.push('\n');
        }
        std::fs::write(dir.join(format!("{id}.jsonl")), content).unwrap();
    }

    fn write_sqlite_session(dir: &Path, id: &str, timestamp: &str, lines: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        let conn = crate::sqlite_backend::open_db(dir).unwrap();
        let header_line = format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"{timestamp}\",\"cwd\":\"/work\"}}"
        );
        let Some(SessionLine::Header(header)) = SessionLine::parse(&header_line) else {
            panic!("header parses");
        };
        crate::sqlite_backend::start_session(&conn, &header).unwrap();
        for line in lines {
            let parsed = SessionLine::parse(line).unwrap();
            crate::sqlite_backend::append_line(&conn, id, &parsed).unwrap();
        }
    }

    #[test]
    fn finds_matches_across_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path();
        let dir_a = agent.join("sessions").join("a");
        let dir_b = agent.join("sessions").join("b");
        write_session(
            &dir_a,
            "s1",
            &[
                "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-08-28T10:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"how do I rotate the frobnicate widget?\",\"timestamp\":1}}",
            ],
        );
        write_session(
            &dir_b,
            "s2",
            &[
                "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":null,\"timestamp\":\"2026-08-28T10:00:02Z\",\"message\":{\"role\":\"user\",\"content\":\"unrelated question\",\"timestamp\":2}}",
            ],
        );

        let hits = search_sessions(agent, "Frobnicate", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
        assert_eq!(hits[0].match_count, 1);
        assert!(
            hits[0].snippets[0].1.contains("frobnicate")
                || hits[0].snippets[0].1.contains("Frobnicate")
        );

        assert!(search_sessions(agent, "missing-term", 10).is_empty());
        assert!(search_sessions(agent, "  ", 10).is_empty());
    }

    /// SQLite-backend sessions (shared `sessions.db`) are indexed next to
    /// JSONL files, with identical match/snippet semantics and recency
    /// ordering across the two sources.
    #[test]
    fn searches_sqlite_backend_alongside_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path();
        let dir_jsonl = agent.join("sessions").join("a");
        write_session(
            &dir_jsonl,
            "jsonl-hit",
            &[
                "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-08-28T10:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"how do I frobnicate from jsonl?\",\"timestamp\":1}}",
            ],
        );
        // Written after the jsonl file, so the db mtime is newer.
        let dir_sql = agent.join("sessions").join("b");
        write_sqlite_session(
            &dir_sql,
            "sql-hit",
            "2026-08-29T10:00:00Z",
            &[
                "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-08-29T10:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"how do I frobnicate from sqlite?\",\"timestamp\":1}}",
                "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":\"m1\",\"timestamp\":\"2026-08-29T10:00:02Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"just frobnicate it\"}],\"api\":\"openai\",\"provider\":\"openai\",\"model\":\"gpt-test\",\"usage\":{\"input\":1,\"output\":1,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":2,\"cost\":{\"input\":0.0,\"output\":0.0,\"cacheRead\":0.0,\"cacheWrite\":0.0,\"total\":0.0}},\"stopReason\":\"stop\",\"timestamp\":2}}",
                "{\"type\":\"message\",\"id\":\"m3\",\"parentId\":\"m2\",\"timestamp\":\"2026-08-29T10:00:03Z\",\"message\":{\"role\":\"user\",\"content\":\"what about the ÜBER drive?\",\"timestamp\":3}}",
            ],
        );
        write_sqlite_session(
            &dir_sql,
            "sql-miss",
            "2026-08-27T10:00:00Z",
            &[
                "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-08-27T10:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"unrelated question\",\"timestamp\":1}}",
            ],
        );

        // Both backends hit; the newer sqlite db sorts first.
        let hits = search_sessions(agent, "Frobnicate", 10);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].session_id, "sql-hit");
        assert_eq!(hits[1].session_id, "jsonl-hit");
        assert!(hits[0].modified >= hits[1].modified);

        let sql = &hits[0];
        assert!(sql.path.ends_with("sessions.db"), "{}", sql.path.display());
        assert_eq!(sql.cwd, "/work");
        assert_eq!(sql.timestamp, "2026-08-29T10:00:00Z");
        assert_eq!(sql.match_count, 2);
        assert_eq!(
            sql.first_prompt.as_deref(),
            Some("how do I frobnicate from sqlite?")
        );
        assert_eq!(sql.snippets.len(), 2);
        assert_eq!(sql.snippets[0].0, "user");
        assert!(sql.snippets[0].1.contains("frobnicate"));
        assert_eq!(sql.snippets[1].0, "assistant");

        // max_results still truncates across sources.
        assert_eq!(search_sessions(agent, "Frobnicate", 1).len(), 1);

        // LIKE wildcards in the query are escaped, not active.
        assert!(search_sessions(agent, "frobnicate%", 10).is_empty());

        // Non-ASCII query: SQLite LIKE cannot fold 'Ü', so the pre-filter
        // is skipped and the Rust matcher (Unicode lowercase) still hits.
        let hits = search_sessions(agent, "über", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "sql-hit");
    }

    /// Regression: case-folding can change byte lengths, so match offsets
    /// computed on the lowercased copy must be mapped back before slicing.
    #[test]
    fn snippet_handles_case_folding_length_changes() {
        // 'İ' (2 bytes) lowercases to "i\u{307}" (3 bytes).
        let text = format!("A{} needle in a haystack", "İ".repeat(100));
        let s = snippet(&text, "needle", 80).unwrap();
        assert!(s.contains("needle in a haystack"), "{s}");
        assert!(s.starts_with('…'));

        // 'ẞ' (3 bytes) lowercases to 'ß' (2 bytes) — previously panicked
        // ("byte index is not a char boundary").
        let s = snippet("ẞẞẞ hello world foo bar baz quux", "hello", 2).unwrap();
        assert!(s.contains("hello"), "{s}");

        // Query at the very start/end of the text.
        assert_eq!(snippet("hello", "hello", 80).as_deref(), Some("hello"));
        assert!(snippet("tail end", "end", 2).unwrap().ends_with("end"));
        assert!(snippet("nothing here", "absent", 5).is_none());
    }
}
