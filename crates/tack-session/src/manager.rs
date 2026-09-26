//! SessionManager: JSONL v3 session persistence. Port of the core of
//! `session-manager.ts` (create/open/continue, append_*, branch, context).

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tack_agent_core::AgentMessage;
use tack_ai::Usage;

use crate::context::{
    SessionContext, build_context_entries, build_session_context, build_session_path, generate_id,
    millis_to_iso, new_session_id, now_iso,
};
use crate::entry::{CURRENT_SESSION_VERSION, SessionEntry, SessionHeader, SessionLine};
use crate::fork_policy::NS_BRANCH_TIP;
use crate::v4::{
    ForkOptions, V4Header, V4NewWrite, V4Store,
    codec::{ParsedSessionHeader, parse_session_header},
};
use crate::v4_bridge::{self, LaneTracker, MAIN_BRANCH};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session file has no header: {0}")]
    MissingHeader(PathBuf),
    #[error("entry not found: {0}")]
    EntryNotFound(String),
    /// The file carries encrypted entry lines but they cannot be decrypted
    /// (no session key installed, wrong key, or tampered ciphertext).
    /// Opening it anyway would silently drop the encrypted history and
    /// keep appending with a wrong parent chain — fail loudly instead.
    #[error(
        "session file contains undecryptable encrypted entries (session key missing or wrong?): {0}"
    )]
    Encrypted(PathBuf),
    /// At-rest encryption of an entry failed: the line is NOT written
    /// (never degrade to plaintext silently).
    #[error("session entry encryption failed; entry not written")]
    EncryptionFailed,
    /// The file is format v4 but the legacy v3 backend was requested
    /// (`sessionBackend: "v3"`): the v3 line parser cannot read v4
    /// transaction logs. Remove the setting or migrate explicitly.
    #[error("session file is format v4 but sessionBackend is \"v3\": {0}")]
    FormatV4(PathBuf),
    /// A v4 storage operation failed.
    #[error("storage error: {0}")]
    V4(#[from] crate::v4::V4Error),
}

/// Default agent dir: `~/.tack/agent` (separate from TS pi's `~/.pi/agent`
/// to avoid concurrent-write corruption; the format is byte-compatible).
pub fn default_agent_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("TACK_AGENT_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".tack")
        .join("agent")
}

/// Session directory for a cwd: `<agentDir>/sessions/--<encoded-cwd>--`
/// (pi's encoding: strip leading slash, replace /\: with -).
pub fn default_session_dir(cwd: &Path, agent_dir: &Path) -> PathBuf {
    let resolved = dunce_canonicalizeish(cwd);
    let mut encoded = resolved.replace(['/', '\\', ':'], "-");
    if let Some(stripped) = encoded.strip_prefix('-') {
        encoded = stripped.to_string();
    }
    agent_dir.join("sessions").join(format!("--{encoded}--"))
}

fn dunce_canonicalizeish(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Session files hold sensitive conversation content: create them 0600 and
/// their directories 0700 on Unix (no-op elsewhere).
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    set_dir_private(path);
    Ok(())
}

#[cfg(unix)]
fn set_dir_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    // Explicit set_permissions (not just a creation mode): create_dir_all
    // applies 0777 & ~umask to NEW dirs and leaves existing ones alone.
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn set_dir_private(_path: &Path) {}

#[cfg(unix)]
fn set_file_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_file_private(_path: &Path) {}

/// Open a session file for appending, creating it 0600 on Unix.
fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Create a file truncated for writing, 0600 on Unix.
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// fsync the parent directory so a rename/create is durable (best-effort:
/// platforms that can't open a directory as a file just skip).
fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

/// Replace a session file's contents without a torn-write window: keep a
/// `.bak` of the original, write a temp file in the same directory, then
/// rename it over the target. A crash mid-rewrite leaves either the old
/// file or the backup intact (previously `fs::write` truncated in place,
/// so power loss during migration destroyed the whole session).
fn atomic_rewrite(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut bak_name = path.as_os_str().to_owned();
    bak_name.push(".bak");
    let bak = PathBuf::from(bak_name);
    // Best-effort backup; a missing/!copyable original must not block the
    // migration itself.
    if std::fs::copy(path, &bak).is_ok() {
        set_file_private(&bak);
    }

    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp_name);
    let write_result = (|| {
        let mut f = create_private_file(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // std rename does not replace an existing target on Windows.
    if let Err(e) = std::fs::rename(&tmp, path) {
        std::fs::remove_file(path)?;
        std::fs::rename(&tmp, path).map_err(|_| e)?;
    }
    set_file_private(path);
    // Without a directory fsync the rename itself can be lost on a crash,
    // resurrecting the pre-rewrite file (and leaving the tmp file behind).
    sync_parent_dir(path);
    Ok(())
}

#[derive(Debug)]
pub struct SessionManager {
    cwd: PathBuf,
    session_dir: PathBuf,
    session_file: Option<PathBuf>,
    header: SessionHeader,
    /// In-memory entry stream (includes the header at [0]). Under the
    /// JsonlV4 backend this holds ONLY the header: the live v4 store is
    /// the single source of truth for entries, so the full history (tool
    /// results, image base64, ...) is not kept twice in memory.
    entries: Vec<SessionLine>,
    /// Id set mirroring the in-memory `entries` (legacy/in-memory
    /// backends only; JsonlV4 answers id queries from the store). Built
    /// once at open and maintained incrementally on every append —
    /// `next_id` previously rebuilt the full set per append, making a
    /// long session O(n²) (F44).
    entry_ids: HashSet<String>,
    leaf_id: Option<String>,
    persist: bool,
    backend: SessionBackend,
    /// Live v4 store (JsonlV4 backend only): appends go through
    /// `V4Store::commit` and entry reads delegate to it (see
    /// [`SessionManager::entries`]).
    v4: Option<V4Store>,
    /// Latest model/thinking level for lane-config mirroring (JsonlV4).
    lane_tracker: LaneTracker,
    /// Mutation counter: bumped on every append, leaf move, and session
    /// reset. Consumers use it to cache derived data (token stats, context
    /// estimates) instead of recomputing them per frame — the recompute
    /// deep-clones every entry and costs O(context size).
    revision: u64,
}

/// Build the entry-id set for a freshly loaded line stream (open paths;
/// appends maintain it incrementally afterwards).
fn collect_entry_ids(lines: &[SessionLine]) -> HashSet<String> {
    lines
        .iter()
        .filter_map(|l| match l {
            SessionLine::Entry(e) => Some(e.id().to_string()),
            _ => None,
        })
        .collect()
}

/// Storage backend: format-v4 transactional JSONL (default), legacy
/// per-session v3 JSONL files (byte-compatible with older TS pi), or the
/// experimental shared SQLite database (`sessions.db`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionBackend {
    /// Legacy format-v3 JSONL (`sessionBackend: "v3"` escape hatch).
    Jsonl,
    /// Format-v4 transactional JSONL (`sessionBackend: "v4"` / unset).
    /// Opening legacy v3/v2/v1 files migrates them transparently.
    #[default]
    JsonlV4,
    /// Experimental shared SQLite backend (`sessionBackend: "sqlite"`).
    Sqlite,
}

impl SessionBackend {
    pub fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("v3") => SessionBackend::Jsonl,
            Some("sqlite") => SessionBackend::Sqlite,
            _ => SessionBackend::JsonlV4,
        }
    }
}

impl SessionManager {
    /// Create a new session (file created on first append).
    pub fn create(cwd: &Path, session_dir: Option<PathBuf>) -> Result<Self, SessionError> {
        Self::create_with_id(cwd, session_dir, new_session_id())
    }

    /// Create a session with an explicit backend (experimental sqlite opt-in).
    pub fn create_with_backend(
        cwd: &Path,
        session_dir: Option<PathBuf>,
        backend: SessionBackend,
    ) -> Result<Self, SessionError> {
        Self::create_inner(cwd, session_dir, new_session_id(), backend)
    }

    /// Create a session with an explicit id (TS `--session-id`: exact id,
    /// created when missing).
    pub fn create_with_id(
        cwd: &Path,
        session_dir: Option<PathBuf>,
        id: String,
    ) -> Result<Self, SessionError> {
        Self::create_inner(cwd, session_dir, id, SessionBackend::default())
    }

    /// Create with explicit id AND backend (sqlite opt-in).
    pub fn create_with_id_and_backend(
        cwd: &Path,
        session_dir: Option<PathBuf>,
        id: String,
        backend: SessionBackend,
    ) -> Result<Self, SessionError> {
        Self::create_inner(cwd, session_dir, id, backend)
    }

    fn create_inner(
        cwd: &Path,
        session_dir: Option<PathBuf>,
        id: String,
        backend: SessionBackend,
    ) -> Result<Self, SessionError> {
        let agent_dir = default_agent_dir();
        let session_dir = session_dir.unwrap_or_else(|| default_session_dir(cwd, &agent_dir));
        let mut manager = SessionManager {
            cwd: cwd.to_path_buf(),
            session_dir,
            session_file: None,
            header: SessionHeader::new(id, now_iso(), cwd.to_string_lossy().to_string(), None),
            entries: Vec::new(),
            entry_ids: HashSet::new(),
            leaf_id: None,
            persist: true,
            backend,
            v4: None,
            lane_tracker: LaneTracker::default(),
            revision: 0,
        };
        manager.start_file(None)?;
        Ok(manager)
    }

    /// In-memory session (no persistence).
    pub fn in_memory(cwd: &Path) -> Self {
        // ONE header, cloned into entries[0]: generating two ids here made
        // session_id() disagree with the header line in the entry stream.
        let header = SessionHeader::new(
            new_session_id(),
            now_iso(),
            cwd.to_string_lossy().to_string(),
            None,
        );
        SessionManager {
            cwd: cwd.to_path_buf(),
            session_dir: PathBuf::new(),
            session_file: None,
            entries: vec![SessionLine::Header(header.clone())],
            header,
            entry_ids: HashSet::new(),
            leaf_id: None,
            persist: false,
            backend: SessionBackend::default(),
            v4: None,
            lane_tracker: LaneTracker::default(),
            revision: 0,
        }
    }

    /// Open a session from the SQLite backend (experimental).
    pub fn open_sqlite(session_id: &str, session_dir: &Path) -> Result<Self, SessionError> {
        let conn = crate::sqlite_backend::open_db(session_dir)?;
        let (header, lines) = crate::sqlite_backend::load_session(&conn, session_id)?;
        let cwd = PathBuf::from(if header.cwd.is_empty() {
            "."
        } else {
            &header.cwd
        });
        let leaf_id = lines.iter().rev().find_map(|l| match l {
            SessionLine::Entry(e) => Some(e.id().to_string()),
            _ => None,
        });
        Ok(SessionManager {
            cwd,
            session_dir: session_dir.to_path_buf(),
            session_file: Some(crate::sqlite_backend::db_path(session_dir)),
            header,
            entry_ids: collect_entry_ids(&lines),
            entries: lines,
            leaf_id,
            persist: true,
            backend: SessionBackend::Sqlite,
            v4: None,
            lane_tracker: LaneTracker::default(),
            revision: 0,
        })
    }

    /// Fork a session within the SQLite backend (copies rows under a new id).
    pub fn fork_from_sqlite(
        from_id: &str,
        target_cwd: &Path,
        session_dir: &Path,
    ) -> Result<Self, SessionError> {
        let conn = crate::sqlite_backend::open_db(session_dir)?;
        let (old_header, _) = crate::sqlite_backend::load_session(&conn, from_id)?;
        let header = SessionHeader::new(
            new_session_id(),
            now_iso(),
            target_cwd.to_string_lossy().to_string(),
            Some(old_header.id.clone()),
        );
        crate::sqlite_backend::fork_session(&conn, from_id, &header)?;
        Self::open_sqlite(&header.id, session_dir)
    }

    /// Most recent session in the SQLite backend, if any.
    pub fn continue_recent_sqlite(session_dir: &Path) -> Result<Option<Self>, SessionError> {
        let db = crate::sqlite_backend::db_path(session_dir);
        if !db.exists() {
            return Ok(None);
        }
        let conn = crate::sqlite_backend::open_db(session_dir)?;
        let sessions = crate::sqlite_backend::list_sessions(&conn)?;
        let Some((id, _)) = sessions.into_iter().next() else {
            return Ok(None);
        };
        Ok(Some(Self::open_sqlite(&id, session_dir)?))
    }

    /// Open an existing session file with the default backend
    /// (format v4: legacy v3/v2/v1 files are migrated transparently).
    pub fn open(path: &Path, session_dir: Option<PathBuf>) -> Result<Self, SessionError> {
        Self::open_with_backend(path, session_dir, SessionBackend::default())
    }

    /// Map a v4 storage error into a session error, preserving the
    /// dedicated `Encrypted` variant (callers key off it to prompt for the
    /// session key).
    fn map_v4_err(e: crate::v4::V4Error) -> SessionError {
        match e {
            crate::v4::V4Error::Encrypted(path) => SessionError::Encrypted(path),
            other => SessionError::V4(other),
        }
    }

    /// Open an existing session file, choosing the storage backend.
    ///
    /// - `JsonlV4`: v4 files open directly; legacy v3 (and v2/v1, after a
    ///   v3 rewrite) files are migrated to v4 on open.
    /// - `Jsonl`: legacy v3 behavior — v1/v2 are migrated to v3; a format
    ///   v4 file is rejected with [`SessionError::FormatV4`].
    pub fn open_with_backend(
        path: &Path,
        session_dir: Option<PathBuf>,
        backend: SessionBackend,
    ) -> Result<Self, SessionError> {
        // First-line format sniff (headers are never encrypted).
        let first_line =
            std::io::BufRead::lines(std::io::BufReader::new(std::fs::File::open(path)?))
                .next()
                .transpose()?
                .unwrap_or_default();
        let sniffed = parse_session_header(first_line.trim());
        if backend == SessionBackend::JsonlV4 {
            let fallback_dir = || {
                session_dir
                    .clone()
                    .unwrap_or_else(|| path.parent().map(Path::to_path_buf).unwrap_or_default())
            };
            match sniffed {
                Some(ParsedSessionHeader::V4(_)) | Some(ParsedSessionHeader::LegacyV3(_)) => {
                    let store = V4Store::open(path).map_err(Self::map_v4_err)?;
                    return Self::from_v4_store(store, fallback_dir());
                }
                None => {
                    // v1/v2 (or a non-canonical v3 layout, e.g. header not
                    // on line 1): the legacy open migrates v1/v2 and
                    // persists v3; an already-v3 odd-layout file is
                    // canonicalized below so the v4 migration can read it.
                    let legacy = Self::open_legacy(path, session_dir.clone())?;
                    if legacy.header.version == Some(CURRENT_SESSION_VERSION) {
                        let mut rewritten = SessionLine::Header(legacy.header.clone()).to_json();
                        rewritten.push('\n');
                        for line in &legacy.entries {
                            if matches!(line, SessionLine::Header(_)) {
                                continue;
                            }
                            let out = Self::render_append_line(
                                line,
                                crate::crypto::session_key().is_some(),
                                crate::crypto::encrypt_line,
                            )?;
                            rewritten.push_str(&out);
                            rewritten.push('\n');
                        }
                        atomic_rewrite(path, &rewritten)?;
                    }
                    let store = V4Store::open(path).map_err(Self::map_v4_err)?;
                    return Self::from_v4_store(store, fallback_dir());
                }
            }
        }
        if matches!(sniffed, Some(ParsedSessionHeader::V4(_))) {
            return Err(SessionError::FormatV4(path.to_path_buf()));
        }
        Self::open_legacy(path, session_dir)
    }

    /// Build a manager from a live v4 store (open/fork paths).
    fn from_v4_store(store: V4Store, session_dir: PathBuf) -> Result<Self, SessionError> {
        let header = SessionHeader {
            entry_type: "session".to_string(),
            version: Some(4),
            id: store.header().id.clone(),
            timestamp: millis_to_iso(store.header().created_at),
            cwd: store.header().cwd.clone(),
            parent_session: store
                .header()
                .parent_session_id
                .clone()
                .or_else(|| store.header().legacy_parent_session_path.clone()),
        };
        let cwd = PathBuf::from(if header.cwd.is_empty() {
            "."
        } else {
            &header.cwd
        });
        // No in-memory entry mirror: the store already holds every entry
        // (with full message payloads); `entries()` maps from it on
        // demand. Keeping a converted copy here doubled the resident
        // history for the whole session lifetime.
        let lines = vec![SessionLine::Header(header.clone())];
        let leaf_id = store
            .branch_tip(MAIN_BRANCH)
            .and_then(|tip| tip)
            .or_else(|| store.entries().last().map(|e| e.id().to_string()));
        let lane_tracker = LaneTracker::from_config(store.lane_config(MAIN_BRANCH).as_ref());
        let session_file = Some(store.path().to_path_buf());
        Ok(SessionManager {
            cwd,
            session_dir,
            session_file,
            header,
            entries: lines,
            entry_ids: HashSet::new(),
            leaf_id,
            persist: true,
            backend: SessionBackend::JsonlV4,
            v4: Some(store),
            lane_tracker,
            revision: 0,
        })
    }

    /// Legacy v3 open path (migrating v1/v2 to v3 on load).
    fn open_legacy(path: &Path, session_dir: Option<PathBuf>) -> Result<Self, SessionError> {
        let content = std::fs::read_to_string(path)?;
        // Fail loudly when the file carries encrypted entries we cannot
        // decrypt: parsing would silently skip them (dropping the whole
        // history) and the manager would keep appending onto a wrong
        // parent chain.
        if crate::crypto::has_undecryptable_encrypted_line(&content) {
            return Err(SessionError::Encrypted(path.to_path_buf()));
        }
        let mut lines: Vec<SessionLine> = content.lines().filter_map(SessionLine::parse).collect();

        let header_idx = lines
            .iter()
            .position(|l| matches!(l, SessionLine::Header(_)));
        let Some(header_idx) = header_idx else {
            return Err(SessionError::MissingHeader(path.to_path_buf()));
        };
        let SessionLine::Header(header) = &lines[header_idx] else {
            unreachable!()
        };
        let mut header = header.clone();
        let cwd = PathBuf::from(if header.cwd.is_empty() {
            "."
        } else {
            &header.cwd
        });

        if migrate_to_current(&mut lines) {
            header.version = Some(CURRENT_SESSION_VERSION);
            // Persist the migrated file (pi's _rewriteFile): appends must not
            // reference freshly generated ids that exist only in memory.
            let mut rewritten = lines
                .iter()
                .map(SessionLine::to_json)
                .collect::<Vec<_>>()
                .join("\n");
            rewritten.push('\n');
            if let Err(e) = atomic_rewrite(path, &rewritten) {
                // Read-only file: appends would fail too, so the in-memory
                // migration is still safe to use for this session's lifetime.
                tracing::warn!("failed to persist migrated session {}: {e}", path.display());
            }
        }

        let leaf_id = lines.iter().rev().find_map(|l| match l {
            SessionLine::Entry(e) => Some(e.id().to_string()),
            _ => None,
        });

        let session_dir =
            session_dir.unwrap_or_else(|| path.parent().map(Path::to_path_buf).unwrap_or_default());

        Ok(SessionManager {
            cwd,
            session_dir,
            session_file: Some(path.to_path_buf()),
            header,
            entry_ids: collect_entry_ids(&lines),
            entries: lines,
            leaf_id,
            persist: true,
            backend: SessionBackend::Jsonl,
            v4: None,
            lane_tracker: LaneTracker::default(),
            revision: 0,
        })
    }

    /// Continue the most recent session for `cwd`, or create a new one
    /// (default backend: format v4).
    pub fn continue_recent(cwd: &Path, session_dir: Option<PathBuf>) -> Result<Self, SessionError> {
        Self::continue_recent_with_backend(cwd, session_dir, SessionBackend::default())
    }

    /// [`continue_recent`](Self::continue_recent) with an explicit backend.
    pub fn continue_recent_with_backend(
        cwd: &Path,
        session_dir: Option<PathBuf>,
        backend: SessionBackend,
    ) -> Result<Self, SessionError> {
        let agent_dir = default_agent_dir();
        let dir = session_dir
            .clone()
            .unwrap_or_else(|| default_session_dir(cwd, &agent_dir));
        if let Some(path) = find_most_recent_session(&dir) {
            // On any parse/open failure, fall back to a fresh session —
            // EXCEPT a locked encrypted session: silently starting fresh
            // would hide the existing (unreadable) history from the user.
            match Self::open_with_backend(&path, session_dir.clone(), backend) {
                Ok(manager) => return Ok(manager),
                Err(e @ SessionError::Encrypted(_)) => return Err(e),
                Err(_) => {}
            }
        }
        Self::create_with_backend(cwd, session_dir, backend)
    }

    /// Start a fresh session file (used by create and /new semantics).
    fn start_file(&mut self, parent_session: Option<String>) -> Result<(), SessionError> {
        // Keep the id chosen by create_with_id; regenerate everything else.
        let id = if self.header.id.is_empty() {
            new_session_id()
        } else {
            self.header.id.clone()
        };
        self.header = SessionHeader::new(
            id,
            now_iso(),
            self.cwd.to_string_lossy().to_string(),
            parent_session.clone(),
        );
        self.entries = vec![SessionLine::Header(self.header.clone())];
        self.entry_ids.clear();
        self.leaf_id = None;
        self.revision += 1;

        if self.persist && self.backend == SessionBackend::JsonlV4 {
            create_private_dir(&self.session_dir)?;
            let file_timestamp = self.header.timestamp.replace([':', '.'], "-");
            let path = self
                .session_dir
                .join(format!("{file_timestamp}_{}.jsonl", self.header.id));
            let mut v4_header = V4Header::new(
                self.header.id.clone(),
                self.cwd.to_string_lossy().to_string(),
            );
            if let Some(parent) = &parent_session {
                v4_header.legacy_parent_session_path = Some(parent.clone());
                // Best-effort: resolve the parent file's first line to its
                // session id (both v4 and legacy v3 parents).
                if let Ok(file) = std::fs::File::open(parent) {
                    let mut line = String::new();
                    if std::io::BufRead::read_line(&mut std::io::BufReader::new(file), &mut line)
                        .is_ok()
                        && let Some(parsed) = parse_session_header(line.trim())
                    {
                        v4_header.parent_session_id = Some(match parsed {
                            ParsedSessionHeader::V4(h) => h.id,
                            ParsedSessionHeader::LegacyV3(h) => h.id,
                        });
                    }
                }
            }
            let store = V4Store::create(
                &path,
                v4_header,
                vec![V4NewWrite::ValueSet {
                    namespace: NS_BRANCH_TIP.to_string(),
                    key: MAIN_BRANCH.to_string(),
                    value: Value::Null,
                }],
            )?;
            set_file_private(&path);
            self.v4 = Some(store);
            self.lane_tracker = LaneTracker::default();
            self.session_file = Some(path);
            return Ok(());
        }

        if self.persist && self.backend == SessionBackend::Sqlite {
            let conn = crate::sqlite_backend::open_db(&self.session_dir)?;
            crate::sqlite_backend::start_session(&conn, &self.header)?;
            self.session_file = Some(crate::sqlite_backend::db_path(&self.session_dir));
            return Ok(());
        }

        if self.persist {
            create_private_dir(&self.session_dir)?;
            let file_timestamp = self.header.timestamp.replace([':', '.'], "-");
            self.session_file = Some(
                self.session_dir
                    .join(format!("{file_timestamp}_{}.jsonl", self.header.id)),
            );
            self.append_line(&SessionLine::Header(self.header.clone()))?;
            // Ensure 0600 even when the file pre-existed with looser perms.
            if let Some(file) = &self.session_file {
                set_file_private(file);
            }
            // entries already contains the header; avoid duplicating it in memory.
        }
        Ok(())
    }

    /// Serialize one line for appending. Entry lines encrypt when a session
    /// key is installed; an encryption failure is a hard error — the line
    /// is NOT written rather than degrading at-rest encryption to plaintext
    /// silently. (Injectable encryptor so tests can force the failure arm;
    /// AES-GCM itself only fails on allocation failure.)
    pub(crate) fn render_append_line(
        line: &SessionLine,
        key_installed: bool,
        encrypt: impl FnOnce(&str) -> Option<String>,
    ) -> Result<String, SessionError> {
        let json = line.to_json();
        // The header stays plaintext (listing/search need it).
        if matches!(line, SessionLine::Header(_)) || !key_installed {
            return Ok(json);
        }
        encrypt(&json).ok_or(SessionError::EncryptionFailed)
    }

    fn append_line(&self, line: &SessionLine) -> Result<(), SessionError> {
        if !self.persist {
            return Ok(());
        }
        if self.backend == SessionBackend::Sqlite {
            let conn = crate::sqlite_backend::open_db(&self.session_dir)?;
            return crate::sqlite_backend::append_line(&conn, &self.header.id, line);
        }
        let Some(file) = &self.session_file else {
            return Ok(());
        };
        // At-rest encryption: the header stays plaintext (listing/search need
        // it); entries encrypt when a session key is installed.
        let out = Self::render_append_line(
            line,
            crate::crypto::session_key().is_some(),
            crate::crypto::encrypt_line,
        )?;
        let mut f = open_private_append(file)?;
        // ONE write for the whole line: two separate write_all calls (JSON,
        // then "\n") let another process interleave bytes between them.
        f.write_all(format!("{out}\n").as_bytes())?;
        Ok(())
    }

    fn next_id(&self) -> String {
        if let Some(store) = &self.v4 {
            return store.mint_entry_id();
        }
        generate_id(&|id| self.entry_ids.contains(id))
    }

    fn push_entry(&mut self, mut entry: SessionEntry) -> Result<String, SessionError> {
        entry.set_parent_id(self.leaf_id.clone());
        let id = entry.id().to_string();
        let line = SessionLine::Entry(entry);
        if self.persist && self.backend == SessionBackend::JsonlV4 {
            if let Some(store) = &mut self.v4 {
                let SessionLine::Entry(entry) = &line else {
                    unreachable!()
                };
                let existing_tool_names = store
                    .lane_config(MAIN_BRANCH)
                    .map(|c| c.active_tool_names)
                    .unwrap_or_default();
                let usage_row_id = store.mint_entry_id();
                let writes = v4_bridge::live_append_writes(
                    entry,
                    &mut self.lane_tracker,
                    existing_tool_names,
                    usage_row_id,
                );
                store.commit(writes)?;
            }
        } else {
            self.append_line(&line)?;
        }
        // Under JsonlV4 the committed store entry IS the record; only the
        // legacy/in-memory backends need the in-memory mirror.
        if self.v4.is_none() {
            self.entry_ids.insert(id.clone());
            self.entries.push(line);
        }
        self.leaf_id = Some(id.clone());
        self.revision += 1;
        Ok(id)
    }

    // --- appends -----------------------------------------------------------

    pub fn append_message(&mut self, message: AgentMessage) -> Result<String, SessionError> {
        let entry = SessionEntry::Message {
            id: self.next_id(),
            parent_id: None,
            timestamp: now_iso(),
            message,
        };
        self.push_entry(entry)
    }

    pub fn append_model_change(
        &mut self,
        provider: &str,
        model_id: &str,
    ) -> Result<String, SessionError> {
        self.push_entry(SessionEntry::ModelChange {
            id: self.next_id(),
            parent_id: None,
            timestamp: now_iso(),
            provider: provider.to_string(),
            model_id: model_id.to_string(),
        })
    }

    pub fn append_thinking_level_change(&mut self, level: &str) -> Result<String, SessionError> {
        self.push_entry(SessionEntry::ThinkingLevelChange {
            id: self.next_id(),
            parent_id: None,
            timestamp: now_iso(),
            thinking_level: level.to_string(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_compaction(
        &mut self,
        summary: &str,
        first_kept_entry_id: Option<String>,
        tokens_before: u64,
        retained_tail: Option<Vec<AgentMessage>>,
        details: Option<Value>,
        usage: Option<Usage>,
    ) -> Result<String, SessionError> {
        let timestamp = now_iso();
        // Upstream #9548 (session-manager.ts appendCompaction): record the
        // complete replayed prompt/tool state at this boundary.
        let system_message =
            tack_ai::transcript::get_current_system_message(&self.build_session_context().messages)
                .map(|mut m| {
                    m.timestamp = crate::context::iso_to_millis(&timestamp).unwrap_or(0);
                    m
                });
        self.push_entry(SessionEntry::Compaction {
            id: self.next_id(),
            parent_id: None,
            timestamp,
            summary: summary.to_string(),
            first_kept_entry_id,
            tokens_before,
            retained_tail,
            details,
            usage,
            from_hook: None,
            system_message,
            first_kept_entry_index: None,
        })
    }

    pub fn append_custom_entry(
        &mut self,
        custom_type: &str,
        data: Option<Value>,
    ) -> Result<String, SessionError> {
        self.push_entry(SessionEntry::Custom {
            id: self.next_id(),
            parent_id: None,
            timestamp: now_iso(),
            custom_type: custom_type.to_string(),
            data,
        })
    }

    pub fn append_session_info(&mut self, name: Option<String>) -> Result<String, SessionError> {
        self.push_entry(SessionEntry::SessionInfo {
            id: self.next_id(),
            parent_id: None,
            timestamp: now_iso(),
            name,
        })
    }

    /// Append a branch-summary entry (TS appendBranchSummary): the LLM summary
    /// of the branch that was abandoned by a tree/fork navigation.
    pub fn append_branch_summary(
        &mut self,
        from_id: &str,
        summary: String,
        details: Option<serde_json::Value>,
        usage: Option<tack_ai::Usage>,
    ) -> Result<String, SessionError> {
        self.push_entry(SessionEntry::BranchSummary {
            id: self.next_id(),
            parent_id: None,
            timestamp: now_iso(),
            from_id: from_id.to_string(),
            summary,
            details,
            usage,
            from_hook: None,
        })
    }

    /// Set or clear a label on an entry (empty/None clears).
    pub fn append_label_change(
        &mut self,
        target_id: &str,
        label: Option<String>,
    ) -> Result<String, SessionError> {
        if !self.has_entry(target_id) {
            return Err(SessionError::EntryNotFound(target_id.to_string()));
        }
        let label = label.filter(|l| !l.is_empty());
        self.push_entry(SessionEntry::Label {
            id: self.next_id(),
            parent_id: None,
            timestamp: now_iso(),
            target_id: target_id.to_string(),
            label,
        })
    }

    /// The current label for an entry (latest label change on the active
    /// file wins, regardless of branch).
    pub fn get_label(&self, entry_id: &str) -> Option<String> {
        // Under JsonlV4 the label is a folded current-state value (kept in
        // sync with label entries by the live write path and migration):
        // the latest change wins, same as the mirror scan below.
        if let Some(store) = &self.v4 {
            return store.get_label(entry_id);
        }
        self.entries.iter().rev().find_map(|l| match l {
            SessionLine::Entry(SessionEntry::Label {
                target_id, label, ..
            }) if target_id == entry_id => Some(label.clone()),
            _ => None,
        })?
    }

    /// True when an entry with this id exists (store under JsonlV4, the
    /// in-memory mirror otherwise).
    fn has_entry(&self, entry_id: &str) -> bool {
        match &self.v4 {
            Some(store) => store.get_entry(entry_id).is_some(),
            None => self.entry_ids.contains(entry_id),
        }
    }

    /// Move the leaf to `branch_from_id` and record a summary of the
    /// abandoned path (pi's branchWithSummary).
    #[allow(clippy::too_many_arguments)]
    pub fn branch_with_summary(
        &mut self,
        branch_from_id: Option<&str>,
        summary: &str,
        details: Option<Value>,
        usage: Option<Usage>,
    ) -> Result<String, SessionError> {
        if let Some(id) = branch_from_id
            && !self.has_entry(id)
        {
            return Err(SessionError::EntryNotFound(id.to_string()));
        }
        let from_id = self.leaf_id.clone().unwrap_or_else(|| "root".to_string());
        self.leaf_id = branch_from_id.map(str::to_string);
        self.push_entry(SessionEntry::BranchSummary {
            id: self.next_id(),
            parent_id: None,
            timestamp: now_iso(),
            from_id,
            summary: summary.to_string(),
            details,
            usage,
            from_hook: None,
        })
    }

    /// Aggregate token/cost totals over the active path (assistant usage
    /// plus compaction/branch-summary LLM usage).
    pub fn session_totals(&self) -> Usage {
        let mut total = Usage::zero();
        for entry in self.build_session_path() {
            let usage = match &entry {
                SessionEntry::Message {
                    message: AgentMessage::Assistant(a),
                    ..
                } => {
                    if matches!(
                        a.stop_reason,
                        tack_ai::StopReason::Error | tack_ai::StopReason::Aborted
                    ) {
                        None
                    } else {
                        Some(&a.usage)
                    }
                }
                SessionEntry::Compaction { usage, .. } => usage.as_ref(),
                SessionEntry::BranchSummary { usage, .. } => usage.as_ref(),
                _ => None,
            };
            if let Some(u) = usage {
                total.input += u.input;
                total.output += u.output;
                total.cache_read += u.cache_read;
                total.cache_write += u.cache_write;
                total.total_tokens += u.total_tokens;
                total.cost.input += u.cost.input;
                total.cost.output += u.cost.output;
                total.cost.cache_read += u.cost.cache_read;
                total.cost.cache_write += u.cost.cache_write;
                total.cost.total += u.cost.total;
            }
        }
        total
    }

    // --- queries -----------------------------------------------------------

    pub fn session_id(&self) -> &str {
        &self.header.id
    }

    /// Mutation counter: changes on every append, leaf move, and session
    /// reset. Consumers cache derived data (token stats, context estimates)
    /// keyed by this instead of recomputing them — the recompute walks and
    /// deep-clones the whole session and costs O(context size).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn session_file(&self) -> Option<&Path> {
        self.session_file.as_deref()
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    /// All entries (excluding the header and unknown lines). Under
    /// JsonlV4 these are mapped from the live store — the store is the
    /// only resident copy of the history.
    pub fn entries(&self) -> Vec<SessionEntry> {
        if let Some(store) = &self.v4 {
            return store
                .entries()
                .iter()
                .map(v4_bridge::v4_to_session_entry)
                .collect();
        }
        self.entries
            .iter()
            .filter_map(|l| match l {
                SessionLine::Entry(e) => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    /// Fork a session file (from another project) into a new session with a
    /// fresh id and a parent pointer (pi's forkFrom). The fork is a format
    /// v4 file produced by the policy-driven v4 fork (the source is
    /// migrated in place first when it is a legacy file).
    pub fn fork_from(source_path: &Path, target_cwd: &Path) -> Result<Self, SessionError> {
        let session_dir = default_session_dir(target_cwd, &default_agent_dir());
        Self::fork_from_in(source_path, target_cwd, &session_dir)
    }

    /// `fork_from` with an explicit session directory (tests; avoids the
    /// process-global default agent dir). Default backend: format v4.
    pub fn fork_from_in(
        source_path: &Path,
        target_cwd: &Path,
        session_dir: &Path,
    ) -> Result<Self, SessionError> {
        Self::fork_from_in_with_backend(
            source_path,
            target_cwd,
            session_dir,
            SessionBackend::default(),
        )
    }

    /// [`fork_from_in`](Self::fork_from_in) with an explicit backend.
    /// `JsonlV4` uses the policy-driven v4 fork; `Jsonl` preserves the
    /// legacy v3 copy-semantics (verbatim line copy + v3 header).
    pub fn fork_from_in_with_backend(
        source_path: &Path,
        target_cwd: &Path,
        session_dir: &Path,
        backend: SessionBackend,
    ) -> Result<Self, SessionError> {
        if backend == SessionBackend::JsonlV4 {
            let source = V4Store::open(source_path).map_err(Self::map_v4_err)?;
            let session_dir = session_dir.to_path_buf();
            create_private_dir(&session_dir)?;
            let new_id = new_session_id();
            let file_timestamp = now_iso().replace([':', '.'], "-");
            let new_file = session_dir.join(format!("{file_timestamp}_{new_id}.jsonl"));
            let mut header = V4Header::new(new_id, target_cwd.to_string_lossy().to_string());
            header.parent_session_id = Some(source.header().id.clone());
            let store = crate::v4::run_v4_fork(
                &source,
                &new_file,
                header,
                &ForkOptions::Tree { id: None },
            )?;
            set_file_private(&new_file);
            return Self::from_v4_store(store, session_dir);
        }
        let mut manager = Self::open_with_backend(source_path, None, backend)?;
        let session_dir = session_dir.to_path_buf();

        manager.header = SessionHeader::new(
            new_session_id(),
            now_iso(),
            target_cwd.to_string_lossy().to_string(),
            Some(source_path.to_string_lossy().to_string()),
        );
        manager.cwd = target_cwd.to_path_buf();
        manager.session_dir = session_dir;
        manager.persist = true;

        // Rewrite the file: new header + all existing lines. The header is
        // not guaranteed to be at index 0 (open() tolerates leading
        // unknown/entry lines), so replace it at its actual position
        // instead of clobbering entries[0].
        create_private_dir(&manager.session_dir)?;
        let file_timestamp = manager.header.timestamp.replace([':', '.'], "-");
        let new_file = manager
            .session_dir
            .join(format!("{file_timestamp}_{}.jsonl", manager.header.id));
        let header_pos = manager
            .entries
            .iter()
            .position(|l| matches!(l, SessionLine::Header(_)))
            .expect("open() requires a header");
        manager.entries[header_pos] = SessionLine::Header(manager.header.clone());
        let mut content = String::new();
        for line in &manager.entries {
            // Re-serialize through the same encryption-aware path as
            // append_line: a fork of an encrypted session STAYS encrypted
            // (a bare to_json() would silently degrade it to plaintext),
            // and an encryption failure is a hard error.
            let out = Self::render_append_line(
                line,
                crate::crypto::session_key().is_some(),
                crate::crypto::encrypt_line,
            )?;
            content.push_str(&out);
            content.push('\n');
        }
        // Same crash-safety as migrations (was a bare fs::write: a torn
        // write here destroyed the forked copy).
        atomic_rewrite(&new_file, &content)?;
        manager.session_file = Some(new_file);
        Ok(manager)
    }

    /// Move the leaf to an earlier entry (pi's `branch`).
    pub fn branch(&mut self, entry_id: &str) -> Result<(), SessionError> {
        if !self.has_entry(entry_id) {
            return Err(SessionError::EntryNotFound(entry_id.to_string()));
        }
        self.leaf_id = Some(entry_id.to_string());
        if self.persist
            && self.backend == SessionBackend::JsonlV4
            && let Some(store) = &mut self.v4
        {
            store.commit(vec![V4NewWrite::ValueSet {
                namespace: NS_BRANCH_TIP.to_string(),
                key: MAIN_BRANCH.to_string(),
                value: Value::String(entry_id.to_string()),
            }])?;
        }
        self.revision += 1;
        Ok(())
    }

    pub fn build_session_path(&self) -> Vec<SessionEntry> {
        build_session_path(&self.entries(), self.leaf_id.as_deref())
    }

    pub fn build_context_entries(&self) -> Vec<SessionEntry> {
        build_context_entries(&self.entries(), self.leaf_id.as_deref())
    }

    pub fn build_session_context(&self) -> SessionContext {
        build_session_context(&self.entries(), self.leaf_id.as_deref())
    }

    /// Repair tool calls left dangling by an interrupted run (kill, crash,
    /// power loss between the assistant message and its tool results):
    /// unanswered tool calls of the last assistant message with tool calls
    /// on the active path get synthetic error results appended at the
    /// current leaf, so the transcript is complete again. The result is
    /// marked `"repaired": true` for provenance. Idempotent; returns the
    /// number of results appended.
    ///
    /// Call this when opening a session FOR CONTINUATION (a new run will
    /// append to it). Read-only consumers (search, export, listing) must
    /// not call it — repair is a write.
    pub fn repair_dangling_tool_calls(&mut self) -> Result<usize, SessionError> {
        let path = self.build_session_path();
        // The kill signature is a tail gap: the loop persists a complete
        // tool batch before the next assistant message, so only the LAST
        // assistant message with tool calls can lack results.
        let Some(last_idx) = path.iter().rposition(|e| {
            matches!(e, SessionEntry::Message { message: AgentMessage::Assistant(a), .. } if a.has_tool_calls())
        }) else {
            return Ok(0);
        };
        let SessionEntry::Message {
            message: AgentMessage::Assistant(assistant),
            ..
        } = &path[last_idx]
        else {
            unreachable!("rposition matched an assistant message")
        };
        let answered: std::collections::HashSet<&str> = path[last_idx + 1..]
            .iter()
            .filter_map(|e| match e {
                SessionEntry::Message {
                    message: AgentMessage::ToolResult(t),
                    ..
                } => Some(t.tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        let missing: Vec<(String, String)> = assistant
            .tool_calls()
            .map(|(id, name, _)| (id.to_string(), name.to_string()))
            .filter(|(id, _)| !answered.contains(id.as_str()))
            .collect();
        let mut repaired = 0;
        for (id, name) in missing {
            self.append_message(AgentMessage::ToolResult(tack_ai::ToolResultMessage {
                tool_call_id: id,
                tool_name: name,
                content: vec![tack_ai::InputContentBlock::text(
                    "Tool execution was interrupted: the run ended before this result was recorded.",
                )],
                details: Some(serde_json::json!({ "repaired": true })),
                usage: None,
                is_error: true,
                timestamp: tack_ai::now_millis(),
            }))?;
            repaired += 1;
        }
        Ok(repaired)
    }
}

/// Most recent .jsonl in a session dir (by modification time).
pub fn find_most_recent_session(session_dir: &Path) -> Option<PathBuf> {
    let read = std::fs::read_dir(session_dir).ok()?;
    read.flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .filter_map(|p| {
            let modified = p.metadata().ok()?.modified().ok()?;
            Some((p, modified))
        })
        .max_by_key(|(_, m)| *m)
        .map(|(p, _)| p)
}

/// Resolve a `--session`/`--fork` argument: an existing file path, or a
/// partial session-id prefix matched against the session dir (TS behavior).
pub fn resolve_session_arg(arg: &str, session_dir: &Path) -> Option<PathBuf> {
    let path = PathBuf::from(arg);
    if path.is_file() {
        return Some(path);
    }
    let matches: Vec<PathBuf> = list_sessions(session_dir)
        .into_iter()
        .filter(|s| s.session_id.starts_with(arg))
        .map(|s| s.path)
        .collect();
    if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        None
    }
}

/// Find a session file by its header session id within a session dir.
pub fn find_session_by_id(session_dir: &Path, session_id: &str) -> Option<PathBuf> {
    let read = std::fs::read_dir(session_dir).ok()?;
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        // Only the first line (header) is needed; both v4 and legacy v3
        // header shapes are recognized.
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut line = String::new();
        use std::io::BufRead;
        if std::io::BufReader::new(file).read_line(&mut line).is_err() {
            continue;
        }
        let header_id = match parse_session_header(line.trim_end()) {
            Some(ParsedSessionHeader::V4(h)) => Some(h.id),
            Some(ParsedSessionHeader::LegacyV3(h)) => Some(h.id),
            None => match SessionLine::parse(line.trim_end()) {
                Some(SessionLine::Header(h)) => Some(h.id),
                _ => None,
            },
        };
        if header_id.as_deref() == Some(session_id) {
            return Some(path);
        }
    }
    None
}

/// Summary of a stored session for picker UIs (`/resume`).
#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub session_id: String,
    /// ISO timestamp from the session header.
    pub timestamp: String,
    pub cwd: String,
    /// Display name from the last SessionInfo entry, if any.
    pub name: Option<String>,
    /// First user message text (preview).
    pub first_prompt: Option<String>,
    pub message_count: usize,
    pub modified: std::time::SystemTime,
}

/// List all sessions in a session dir, most recent first.
pub fn list_sessions(session_dir: &Path) -> Vec<SessionSummary> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(session_dir) else {
        return out;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Format v4: shallow scan of the transaction log (substring
        // pre-filter per line, user messages counted without the
        // entry deep-clone — the same latency trick as the v3 path).
        if let Some(scan) = crate::v4_bridge::scan_v4_file_summary(&content) {
            out.push(SessionSummary {
                path: path.clone(),
                session_id: scan.header.id.clone(),
                timestamp: millis_to_iso(scan.header.created_at),
                cwd: scan.header.cwd.clone(),
                name: scan.session_name.or(scan.session_info_name),
                first_prompt: scan.first_prompt,
                message_count: scan.user_message_count,
                modified: entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            });
            continue;
        }
        let mut header: Option<SessionHeader> = None;
        let mut name = None;
        let mut first_prompt = None;
        let mut message_count = 0usize;
        for line in content.lines() {
            // Cheap substring pre-filter: fully JSON-parsing every line of
            // every session file made /resume O(total bytes parsed) — huge
            // sessions dominated the picker latency. Only header /
            // session_info / user-message lines can affect the summary;
            // everything else is skipped on a memchr. (Both compact and
            // spaced `":` serializations are accepted; false positives just
            // cost one parse and are rejected by the match below.)
            let is_header = header.is_none()
                && (line.contains("\"type\":\"session\"")
                    || line.contains("\"type\": \"session\""));
            let is_info = line.contains("\"type\":\"session_info\"")
                || line.contains("\"type\": \"session_info\"");
            let is_user_msg = (line.contains("\"type\":\"message\"")
                || line.contains("\"type\": \"message\""))
                && (line.contains("\"role\":\"user\"") || line.contains("\"role\": \"user\""));
            if !is_header && !is_info && !is_user_msg {
                continue;
            }
            match SessionLine::parse(line) {
                Some(SessionLine::Header(h)) => header = Some(h),
                Some(SessionLine::Entry(SessionEntry::SessionInfo { name: n, .. })) => name = n,
                Some(SessionLine::Entry(SessionEntry::Message {
                    message: AgentMessage::User(u),
                    ..
                })) => {
                    message_count += 1;
                    if first_prompt.is_none() {
                        let text = match &u.content {
                            tack_ai::UserContent::Text(t) => t.clone(),
                            tack_ai::UserContent::Blocks(blocks) => blocks
                                .iter()
                                .filter_map(|b| match b {
                                    tack_ai::InputContentBlock::Text { text, .. } => {
                                        Some(text.as_str())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join(" "),
                        };
                        first_prompt = Some(text);
                    }
                }
                _ => {}
            }
        }
        if let Some(h) = header {
            out.push(SessionSummary {
                path: path.clone(),
                session_id: h.id,
                timestamp: h.timestamp,
                cwd: h.cwd,
                name,
                first_prompt,
                message_count,
                modified: entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            });
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

/// Migrate v1→v2→v3 in place (pi's migrateToCurrentVersion). Returns true
/// when a migration was applied (the caller then rewrites the file, like
/// pi's `_rewriteFile`, so appended entries never reference the freshly
/// generated in-memory-only ids).
fn migrate_to_current(lines: &mut [SessionLine]) -> bool {
    let version = lines
        .iter()
        .find_map(|l| match l {
            SessionLine::Header(h) => h.version,
            _ => None,
        })
        .unwrap_or(1);
    if version >= CURRENT_SESSION_VERSION {
        return false;
    }

    if version < 2 {
        // v1 → v2: linear entries gain id/parentId links.
        let mut ids: HashSet<String> = HashSet::new();
        let mut prev_id: Option<String> = None;
        for line in lines.iter_mut() {
            match line {
                SessionLine::Header(h) => h.version = Some(2),
                SessionLine::Entry(e) => {
                    let id = generate_id(&|id: &str| ids.contains(id));
                    ids.insert(id.clone());
                    e.set_id(id.clone());
                    e.set_parent_id(prev_id.clone());
                    prev_id = Some(id);
                }
                SessionLine::Unknown(_) => {}
            }
        }
        // firstKeptEntryIndex → firstKeptEntryId. The index refers to the
        // full line list (header included), matching TS `entries[i]`.
        for i in 0..lines.len() {
            let index = match &lines[i] {
                SessionLine::Entry(SessionEntry::Compaction {
                    first_kept_entry_index,
                    ..
                }) => *first_kept_entry_index,
                _ => None,
            };
            let Some(index) = index else { continue };
            let target_id = lines.get(index).and_then(|l| match l {
                SessionLine::Entry(e) => Some(e.id().to_string()),
                _ => None,
            });
            if let SessionLine::Entry(SessionEntry::Compaction {
                first_kept_entry_id,
                first_kept_entry_index,
                ..
            }) = &mut lines[i]
            {
                if let Some(id) = target_id {
                    *first_kept_entry_id = Some(id);
                }
                *first_kept_entry_index = None;
            }
        }
    }

    if version < 3 {
        for line in lines.iter_mut() {
            if let SessionLine::Header(h) = line {
                h.version = Some(3);
            }
        }
        // hookMessage role → custom is handled at parse time
        // (SessionLine::from_value rewrites the role before decoding).
    }
    true
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Regression: fork_from used to stamp the new header over entries[0],
    /// which destroys a leading entry and leaves TWO headers when the
    /// source file's header isn't the first line (open() tolerates that).
    #[test]
    fn fork_from_replaces_header_at_its_actual_position() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("odd.jsonl");
        let content = concat!(
            "{\"type\":\"message\",\"id\":\"m0\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"first\",\"timestamp\":1}}\n",
            "{\"type\":\"session\",\"version\":3,\"id\":\"old-id\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/work\"}\n",
            "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":\"m0\",\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"second\",\"timestamp\":2}}\n",
        );
        std::fs::write(&source, content).unwrap();

        let fork = SessionManager::fork_from_in_with_backend(
            &source,
            tmp.path(),
            tmp.path(),
            SessionBackend::Jsonl,
        )
        .unwrap();
        assert_ne!(fork.session_id(), "old-id");

        let written = std::fs::read_to_string(fork.session_file().unwrap()).unwrap();
        let lines: Vec<SessionLine> = written.lines().filter_map(SessionLine::parse).collect();
        let header_count = lines
            .iter()
            .filter(|l| matches!(l, SessionLine::Header(_)))
            .count();
        assert_eq!(header_count, 1, "exactly one header: {written}");
        assert!(
            matches!(&lines[0], SessionLine::Entry(e) if e.id() == "m0"),
            "leading entry preserved: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| matches!(l, SessionLine::Entry(e) if e.id() == "m1"))
        );
        if let Some(SessionLine::Header(h)) =
            lines.iter().find(|l| matches!(l, SessionLine::Header(_)))
        {
            assert_eq!(h.id, fork.session_id());
            assert_eq!(
                h.parent_session.as_deref(),
                Some(source.to_string_lossy().as_ref())
            );
        }
    }

    /// Regression: fork_from_in used to re-serialize entries with a bare
    /// to_json(), silently degrading a forked encrypted session to
    /// plaintext. The fork must re-encrypt entry lines (header stays
    /// plaintext) and decrypt back through the normal open path.
    #[test]
    fn fork_of_encrypted_session_stays_encrypted() {
        crate::crypto::install_test_key();
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("encrypted.jsonl");
        let entry_json = "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"fork secret\",\"timestamp\":1}}";
        let encrypted = crate::crypto::encrypt_line(entry_json).unwrap();
        let content = format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/work\"}}\n{encrypted}\n"
        );
        std::fs::write(&source, &content).unwrap();

        let fork_dir = tmp.path().join("fork-sessions");
        let fork = SessionManager::fork_from_in_with_backend(
            &source,
            tmp.path(),
            &fork_dir,
            SessionBackend::Jsonl,
        )
        .unwrap();

        // No plaintext leak; the entry line is ciphertext, the header is
        // still plaintext JSON (listing/search need it).
        let written = std::fs::read_to_string(fork.session_file().unwrap()).unwrap();
        assert!(
            !written.contains("fork secret"),
            "plaintext leaked: {written}"
        );
        let entry_lines: Vec<&str> = written
            .lines()
            .filter(|l| !l.contains("\"type\":\"session\""))
            .collect();
        assert_eq!(entry_lines.len(), 1, "{written}");
        assert!(
            crate::crypto::is_encrypted_line(entry_lines[0]),
            "forked entry line must stay encrypted: {written}"
        );

        // ... and the fork decrypts back through SessionManager::open.
        let reopened = SessionManager::open_with_backend(
            fork.session_file().unwrap(),
            None,
            SessionBackend::Jsonl,
        )
        .unwrap();
        let entries = reopened.entries();
        assert_eq!(entries.len(), 1);
        let SessionEntry::Message {
            message: AgentMessage::User(u),
            ..
        } = &entries[0]
        else {
            panic!("expected user message, got {:?}", entries[0]);
        };
        let tack_ai::UserContent::Text(text) = &u.content else {
            panic!("expected text content");
        };
        assert_eq!(text, "fork secret");
    }

    /// Regression: in_memory used to call new_session_id() twice (once
    /// for the header field, once for entries[0]), so session_id()
    /// disagreed with the header line in the entry stream.
    #[test]
    fn in_memory_session_id_matches_entry_stream_header() {
        let manager = SessionManager::in_memory(Path::new("/work"));
        let Some(SessionLine::Header(stream_header)) = manager.entries.first() else {
            panic!("entries[0] must be the header");
        };
        assert_eq!(manager.session_id(), stream_header.id);
        assert_eq!(manager.session_id(), manager.header.id);
    }

    /// v1 session files (no id/parentId on entries, header without version)
    /// must be migrated on open: entries gain linked ids, compaction
    /// firstKeptEntryIndex resolves to firstKeptEntryId, and the migrated
    /// file is persisted so later appends never dangle (TS _rewriteFile).
    #[test]
    fn v1_session_is_migrated_and_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("v1.jsonl");
        let content = concat!(
            "{\"type\":\"session\",\"id\":\"s1\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/work\"}\n",
            "{\"type\":\"message\",\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hello\",\"timestamp\":1}}\n",
            "{\"type\":\"message\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"message\":{\"role\":\"user\",\"content\":\"world\",\"timestamp\":2}}\n",
            "{\"type\":\"compaction\",\"timestamp\":\"2024-01-01T00:00:03Z\",\"summary\":\"sum\",\"firstKeptEntryIndex\":2,\"tokensBefore\":100}\n",
        );
        std::fs::write(&file, content).unwrap();

        let manager =
            SessionManager::open_with_backend(&file, None, SessionBackend::Jsonl).unwrap();
        let entries = manager.entries();
        assert_eq!(
            entries.len(),
            3,
            "v1 entries must parse, not land in Unknown"
        );

        // Unique non-empty ids, linked parent chain.
        let ids: std::collections::HashSet<&str> = entries.iter().map(|e| e.id()).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.iter().all(|id| !id.is_empty()));
        assert_eq!(entries[0].parent_id(), None);
        assert_eq!(entries[1].parent_id(), Some(entries[0].id()));
        assert_eq!(entries[2].parent_id(), Some(entries[1].id()));

        // firstKeptEntryIndex=2 → the second message (header is index 0).
        let crate::entry::SessionEntry::Compaction {
            first_kept_entry_id,
            first_kept_entry_index,
            ..
        } = &entries[2]
        else {
            panic!("expected compaction, got {:?}", entries[2]);
        };
        assert_eq!(first_kept_entry_id.as_deref(), Some(entries[1].id()));
        assert!(
            first_kept_entry_index.is_none(),
            "index consumed by migration"
        );

        // Persisted: the file on disk carries the migrated ids/version, so
        // reopening does not re-migrate (and appends cannot dangle).
        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert!(on_disk.contains("\"version\":3"), "{on_disk}");
        assert!(!on_disk.contains("firstKeptEntryIndex"), "{on_disk}");
        let manager2 =
            SessionManager::open_with_backend(&file, None, SessionBackend::Jsonl).unwrap();
        let ids2: Vec<String> = manager2
            .entries()
            .iter()
            .map(|e| e.id().to_string())
            .collect();
        assert_eq!(
            ids2,
            entries
                .iter()
                .map(|e| e.id().to_string())
                .collect::<Vec<_>>()
        );

        // The migration keeps a .bak of the pre-migration file.
        let bak = PathBuf::from(format!("{}.bak", file.display()));
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), content);
        // No temp file is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files cleaned up: {leftovers:?}");
    }

    /// v2 → v3: hookMessage roles are renamed to custom (parse-time hook
    /// in SessionLine::from_value) and the rename is persisted by the
    /// migration rewrite, so the old role never reappears on disk. Existing
    /// entry ids are NOT regenerated (only v1 files gain ids).
    #[test]
    fn v2_session_with_hook_message_is_migrated_to_custom() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("v2.jsonl");
        let content = concat!(
            "{\"type\":\"session\",\"version\":2,\"id\":\"s2\",\"timestamp\":\"2025-06-01T00:00:00Z\",\"cwd\":\"/work\"}\n",
            "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2025-06-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
            "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":\"m1\",\"timestamp\":\"2025-06-01T00:00:02Z\",\"message\":{\"role\":\"hookMessage\",\"customType\":\"hook:session-init\",\"content\":\"hook ran ok\",\"display\":true,\"timestamp\":2}}\n",
        );
        std::fs::write(&file, content).unwrap();

        let manager =
            SessionManager::open_with_backend(&file, None, SessionBackend::Jsonl).unwrap();
        let entries = manager.entries();
        assert_eq!(entries.len(), 2, "hookMessage entry must decode, not drop");

        // v2 → v3 must not touch ids or the parent chain.
        assert_eq!(entries[0].id(), "m1");
        assert_eq!(entries[1].id(), "m2");
        assert_eq!(entries[1].parent_id(), Some("m1"));

        // The hookMessage decoded as a custom message with content intact.
        let SessionEntry::Message {
            message: AgentMessage::Custom(custom),
            ..
        } = &entries[1]
        else {
            panic!("expected custom message, got {:?}", entries[1]);
        };
        assert_eq!(custom.custom_type, "hook:session-init");
        assert!(custom.display);
        let tack_ai::UserContent::Text(text) = &custom.content else {
            panic!("expected text content, got {:?}", custom.content);
        };
        assert_eq!(text, "hook ran ok");

        // Persisted: the rewritten file is v3 and carries no hookMessage.
        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert!(on_disk.contains("\"version\":3"), "{on_disk}");
        assert!(on_disk.contains("\"role\":\"custom\""), "{on_disk}");
        assert!(!on_disk.contains("hookMessage"), "{on_disk}");

        // Reopening parses the custom message as-is (no second migration).
        let manager2 =
            SessionManager::open_with_backend(&file, None, SessionBackend::Jsonl).unwrap();
        assert_eq!(manager2.entries(), entries);
    }

    /// Full chain: a v1 file (no version, no ids) migrates straight to v3
    /// in one open() — ids/parent links assigned (v1→v2 step), header
    /// stamped v3 — and the migration is idempotent: reopening the
    /// persisted file must not rewrite it again.
    #[test]
    fn v1_session_migrates_to_v3_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("v1.jsonl");
        let content = concat!(
            "{\"type\":\"session\",\"id\":\"s1\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/work\"}\n",
            "{\"type\":\"message\",\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"one\",\"timestamp\":1}}\n",
            "{\"type\":\"message\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"message\":{\"role\":\"user\",\"content\":\"two\",\"timestamp\":2}}\n",
        );
        std::fs::write(&file, content).unwrap();

        let manager =
            SessionManager::open_with_backend(&file, None, SessionBackend::Jsonl).unwrap();
        let entries = manager.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].parent_id(), None);
        assert_eq!(entries[1].parent_id(), Some(entries[0].id()));
        let ids: Vec<String> = entries.iter().map(|e| e.id().to_string()).collect();
        assert!(ids.iter().all(|id| !id.is_empty()));

        // Persisted as v3 in one pass (no intermediate v2 file left).
        let after_first = std::fs::read_to_string(&file).unwrap();
        assert!(after_first.contains("\"version\":3"), "{after_first}");
        assert!(!after_first.contains("\"version\":2"), "{after_first}");

        // Idempotent: remove the migration backup, reopen, and verify the
        // file is byte-identical and no new backup was written (i.e.
        // migrate_to_current returned false and no rewrite happened).
        let bak = PathBuf::from(format!("{}.bak", file.display()));
        std::fs::remove_file(&bak).unwrap();
        let manager2 =
            SessionManager::open_with_backend(&file, None, SessionBackend::Jsonl).unwrap();
        let after_second = std::fs::read_to_string(&file).unwrap();
        assert_eq!(after_first, after_second, "second open must not rewrite");
        assert!(!bak.exists(), "no rewrite -> no new .bak");
        let ids2: Vec<String> = manager2
            .entries()
            .iter()
            .map(|e| e.id().to_string())
            .collect();
        assert_eq!(ids, ids2, "ids are stable across reopens");
    }

    /// Unknown/future entry lines must survive migration rewrites — they
    /// are not SessionEntry values, so every migration step must pass them
    /// through untouched, both on disk and across repeated opens.
    #[test]
    fn migration_preserves_unknown_future_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("v1-unknown.jsonl");
        let future_line = "{\"type\":\"quantum_checkpoint\",\"payload\":{\"state\":[1,2,3],\"note\":\"from the future\"}}";
        let content = format!(
            "{{\"type\":\"session\",\"id\":\"s1\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/work\"}}\n\
             {{\"type\":\"message\",\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{{\"role\":\"user\",\"content\":\"before\",\"timestamp\":1}}}}\n\
             {future_line}\n\
             {{\"type\":\"message\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"message\":{{\"role\":\"user\",\"content\":\"after\",\"timestamp\":2}}}}\n"
        );
        std::fs::write(&file, &content).unwrap();

        let manager =
            SessionManager::open_with_backend(&file, None, SessionBackend::Jsonl).unwrap();
        let entries = manager.entries();
        assert_eq!(entries.len(), 2, "unknown line is not an entry");
        // The v1→v2 id-linking pass skips Unknown lines: the parent chain
        // links across the unknown line.
        assert_eq!(entries[1].parent_id(), Some(entries[0].id()));

        // The unknown line survived the persisted rewrite, payload intact.
        let on_disk = std::fs::read_to_string(&file).unwrap();
        let disk_lines: Vec<SessionLine> = on_disk.lines().filter_map(SessionLine::parse).collect();
        let unknowns: Vec<&serde_json::Value> = disk_lines
            .iter()
            .filter_map(|l| match l {
                SessionLine::Unknown(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(unknowns.len(), 1, "{on_disk}");
        assert_eq!(
            unknowns[0],
            &serde_json::from_str::<serde_json::Value>(future_line).unwrap()
        );

        // And it survives a second open (idempotent no-rewrite path).
        let manager2 =
            SessionManager::open_with_backend(&file, None, SessionBackend::Jsonl).unwrap();
        assert_eq!(manager2.entries().len(), 2);
        let on_disk2 = std::fs::read_to_string(&file).unwrap();
        assert!(on_disk2.contains("quantum_checkpoint"), "{on_disk2}");
    }

    /// atomic_rewrite must replace contents and keep a backup of the
    /// original (the migration path's crash-safety net).
    #[test]
    fn atomic_rewrite_replaces_and_backs_up() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("s.jsonl");
        std::fs::write(&file, "original\n").unwrap();
        atomic_rewrite(&file, "rewritten\n").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "rewritten\n");
        let bak = PathBuf::from(format!("{}.bak", file.display()));
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "original\n");
    }

    /// Session files hold sensitive conversation content: 0600 files,
    /// 0700 session dir, and the same for .bak/.tmp artifacts.
    #[cfg(unix)]
    #[test]
    fn session_files_and_dirs_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut manager = SessionManager::create(tmp.path(), Some(session_dir.clone())).unwrap();
        assert_eq!(mode(&session_dir), 0o700, "session dir must be 0700");

        manager
            .append_message(AgentMessage::user("sensitive content"))
            .unwrap();
        let file = manager.session_file().unwrap().to_path_buf();
        assert_eq!(mode(&file), 0o600, "session file must be 0600");

        // fork_from_in: the forked file is created via atomic_rewrite in a
        // private dir.
        let fork_dir = tmp.path().join("fork-sessions");
        let fork = SessionManager::fork_from_in(&file, tmp.path(), &fork_dir).unwrap();
        assert_eq!(mode(&fork_dir), 0o700, "fork session dir must be 0700");
        assert_eq!(
            mode(fork.session_file().unwrap()),
            0o600,
            "fork file must be 0600"
        );

        // Loosen perms externally: the next rewrite must re-tighten them,
        // and the .bak must be private too.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        atomic_rewrite(&file, "rewritten\n").unwrap();
        assert_eq!(mode(&file), 0o600, "rewritten file must be 0600");
        let bak = PathBuf::from(format!("{}.bak", file.display()));
        assert_eq!(mode(&bak), 0o600, ".bak must be 0600");
    }

    /// Encrypted entries without a usable key must fail open() loudly
    /// (SessionError::Encrypted), not drop the history and keep appending
    /// onto a wrong parent chain.
    #[test]
    fn open_rejects_undecryptable_encrypted_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("locked.jsonl");
        // Undecryptable regardless of key state (invalid base64 payload):
        // with no key installed this is exactly the "encrypted but no key"
        // case; with a key it's the wrong-key/tampered case.
        let content = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/work\"}\n",
            "tack-enc:v1:not-valid-base64!!!\n",
        );
        std::fs::write(&file, content).unwrap();

        let err = SessionManager::open(&file, None).unwrap_err();
        assert!(matches!(err, SessionError::Encrypted(_)), "{err:?}");

        // continue_recent must propagate the lock instead of silently
        // starting a fresh session over the unreadable history.
        let dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&file, dir.join("locked.jsonl")).unwrap();
        let err = SessionManager::continue_recent(tmp.path(), Some(dir)).unwrap_err();
        assert!(matches!(err, SessionError::Encrypted(_)), "{err:?}");
    }

    /// Plaintext sessions still open fine (no false positive from the
    /// encrypted-line guard).
    #[test]
    fn open_allows_plaintext_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("plain.jsonl");
        let content = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/work\"}\n",
            "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}\n",
        );
        std::fs::write(&file, content).unwrap();
        let manager = SessionManager::open(&file, None).unwrap();
        assert_eq!(manager.entries().len(), 1);
    }

    /// Encryption failure must be a hard error — never a silent plaintext
    /// fallback (the render step is what append_line uses).
    #[test]
    fn encryption_failure_is_an_error_not_plaintext_fallback() {
        let entry = SessionLine::Entry(SessionEntry::SessionInfo {
            id: "e1".to_string(),
            parent_id: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            name: Some("secret".to_string()),
        });
        let err = SessionManager::render_append_line(&entry, true, |_| None).unwrap_err();
        assert!(matches!(err, SessionError::EncryptionFailed), "{err:?}");

        // No key installed -> plaintext pass-through (encryption off).
        let plain = SessionManager::render_append_line(&entry, false, |_| unreachable!()).unwrap();
        assert!(plain.contains("secret"));

        // The header is never encrypted.
        let header = SessionLine::Header(SessionHeader::new(
            "s1".to_string(),
            "2026-01-01T00:00:00Z".to_string(),
            "/work".to_string(),
            None,
        ));
        let plain = SessionManager::render_append_line(&header, true, |_| unreachable!()).unwrap();
        assert!(plain.contains("\"type\":\"session\""));
    }
}
