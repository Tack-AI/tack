//! Built-in Herdr integration for interactive tack sessions.
//!
//! Herdr owns the terminal, while tack reports semantic agent state and a
//! session-resume command back to it. The CLI transport keeps Windows named
//! pipe handling in Herdr and makes a missing or older Herdr harmless.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const SOURCE: &str = "tack:herdr";
const AGENT: &str = "tack";
const REPORT_TIMEOUT: Duration = Duration::from_millis(750);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RESUME_ARGS: usize = 64;
const MAX_RESUME_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HerdrState {
    Idle,
    Working,
    Blocked,
}

impl HerdrState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Debug)]
struct Config {
    binary: PathBuf,
    pane_id: String,
}

impl Config {
    fn from_env() -> Option<Self> {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Option<Self> {
        if lookup("HERDR_ENV").as_deref() != Some(OsStr::new("1")) {
            return None;
        }
        // The CLI inherits this variable. Requiring it prevents a stray pane
        // id from accidentally reporting to an unrelated default server.
        if lookup("HERDR_SOCKET_PATH").is_none_or(|value| value.is_empty()) {
            return None;
        }
        let binary = lookup("HERDR_BIN_PATH")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)?;
        let pane_id = lookup("HERDR_PANE_ID")?
            .into_string()
            .ok()
            .filter(|value| !value.trim().is_empty())?;
        Some(Self { binary, pane_id })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SessionSnapshot {
    id: String,
    directory: PathBuf,
    agent_directory: PathBuf,
    provider: String,
    model: String,
    thinking: Option<String>,
}

#[derive(Debug)]
enum Report {
    State {
        state: HerdrState,
        message: Option<String>,
        seq: u64,
    },
    Session {
        session_id: String,
        resume_argv: Option<Vec<String>>,
        seq: u64,
    },
    Release {
        seq: u64,
        ack: Option<oneshot::Sender<()>>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CoalesceGroup {
    State,
    Session,
}

impl Report {
    fn coalesce_group(&self) -> Option<CoalesceGroup> {
        match self {
            Self::State { .. } => Some(CoalesceGroup::State),
            Self::Session { .. } => Some(CoalesceGroup::Session),
            Self::Release { .. } => None,
        }
    }
}

#[derive(Debug)]
struct Client {
    tx: mpsc::UnboundedSender<Report>,
    worker: Option<JoinHandle<()>>,
}

/// A deduplicating handle to the background Herdr reporter.
///
/// Calls never await the Herdr process on the UI path. Only `release` waits,
/// with a bound, so shutdown can deliver the final report where possible.
#[derive(Debug)]
pub(crate) struct HerdrReporter {
    client: Option<Client>,
    last_state: Option<(HerdrState, Option<String>)>,
    last_session: Option<SessionSnapshot>,
    seq: u64,
}

impl Default for HerdrReporter {
    fn default() -> Self {
        Self {
            client: None,
            last_state: None,
            last_session: None,
            seq: initial_sequence(),
        }
    }
}

impl HerdrReporter {
    pub(crate) fn from_env() -> Self {
        // Unit tests must never report into a developer's real Herdr pane.
        // Keep the production path type-checked in test builds as well.
        if cfg!(test) {
            Self::default()
        } else {
            Self::from_config(Config::from_env())
        }
    }

    fn from_config(config: Option<Config>) -> Self {
        let mut reporter = Self::default();
        if let Some(config) = config {
            let (tx, rx) = mpsc::unbounded_channel();
            let worker = tokio::spawn(run_worker(config, rx));
            reporter.client = Some(Client {
                tx,
                worker: Some(worker),
            });
        }
        reporter
    }

    pub(crate) fn report_state(&mut self, state: HerdrState, message: Option<&str>) {
        let Some(tx) = self.client.as_ref().map(|client| client.tx.clone()) else {
            return;
        };
        if self
            .last_state
            .as_ref()
            .is_some_and(|(last_state, last_message)| {
                *last_state == state && last_message.as_deref() == message
            })
        {
            return;
        }
        self.last_state = Some((state, message.map(str::to_string)));
        let _ = tx.send(Report::State {
            state,
            message: message.map(str::to_string),
            seq: self.next_sequence(),
        });
    }

    pub(crate) fn report_session(
        &mut self,
        session_id: &str,
        session_directory: &Path,
        agent_directory: &Path,
        provider: &str,
        model: &str,
        thinking: Option<&str>,
    ) {
        let Some(tx) = self.client.as_ref().map(|client| client.tx.clone()) else {
            return;
        };
        if self.last_session.as_ref().is_some_and(|last| {
            last.id == session_id
                && last.directory == session_directory
                && last.agent_directory == agent_directory
                && last.provider == provider
                && last.model == model
                && last.thinking.as_deref() == thinking
        }) {
            return;
        }
        let identity = SessionSnapshot {
            id: session_id.to_string(),
            directory: session_directory.to_path_buf(),
            agent_directory: agent_directory.to_path_buf(),
            provider: provider.to_string(),
            model: model.to_string(),
            thinking: thinking.map(str::to_string),
        };
        let resume_argv = resume_argv(
            session_id,
            session_directory,
            agent_directory,
            provider,
            model,
            thinking,
        );
        if resume_argv.is_none() {
            tracing::debug!(
                "Herdr session report omits an invalid resume command for session {session_id}"
            );
        }
        self.last_session = Some(identity);
        let _ = tx.send(Report::Session {
            session_id: session_id.to_string(),
            resume_argv,
            seq: self.next_sequence(),
        });
    }

    pub(crate) async fn release(&mut self) {
        let Some(mut client) = self.client.take() else {
            return;
        };
        let (ack, delivered) = oneshot::channel();
        if client
            .tx
            .send(Report::Release {
                seq: self.next_sequence(),
                ack: Some(ack),
            })
            .is_ok()
        {
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, delivered).await;
        }
        drop(client.tx);
        if let Some(worker) = client.worker.take() {
            worker.abort();
        }
    }

    fn next_sequence(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    #[cfg(test)]
    fn for_test() -> (Self, mpsc::UnboundedReceiver<Report>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let reporter = Self {
            client: Some(Client { tx, worker: None }),
            ..Self::default()
        };
        (reporter, rx)
    }
}

fn initial_sequence() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis)
        .unwrap_or(u64::MAX)
        .saturating_mul(1000)
}

fn resume_argv(
    session_id: &str,
    session_directory: &Path,
    agent_directory: &Path,
    provider: &str,
    model: &str,
    thinking: Option<&str>,
) -> Option<Vec<String>> {
    let mut argv = vec![
        "tack".to_string(),
        "--agent-dir".to_string(),
        agent_directory.to_str()?.to_string(),
        "--session".to_string(),
        session_id.to_string(),
        "--continue".to_string(),
        "--session-dir".to_string(),
        session_directory.to_str()?.to_string(),
        "--provider".to_string(),
        provider.to_string(),
        "--model".to_string(),
        model.to_string(),
    ];
    if let Some(thinking) = thinking {
        argv.push("--thinking".to_string());
        argv.push(thinking.to_string());
    }
    valid_resume_argv(&argv).then_some(argv)
}

fn valid_resume_argv(argv: &[String]) -> bool {
    !argv.is_empty()
        && argv.len() <= MAX_RESUME_ARGS
        && argv[0] == "tack"
        && argv
            .iter()
            .all(|arg| !arg.contains('\'') && !arg.chars().any(char::is_control))
        && argv.iter().map(|arg| arg.len() + 1).sum::<usize>() <= MAX_RESUME_BYTES
}

async fn run_worker(config: Config, mut rx: mpsc::UnboundedReceiver<Report>) {
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while let Ok(report) = rx.try_recv() {
            batch.push(report);
        }
        for report in coalesce_batch(batch) {
            execute_report(&config, report).await;
        }
    }
}

fn coalesce_batch(batch: Vec<Report>) -> Vec<Report> {
    let mut coalesced = Vec::with_capacity(batch.len());
    let mut reports = batch.into_iter().peekable();
    while let Some(mut report) = reports.next() {
        if let Some(group) = report.coalesce_group() {
            while reports
                .peek()
                .is_some_and(|next| next.coalesce_group() == Some(group))
            {
                report = reports.next().expect("peeked report");
            }
        }
        coalesced.push(report);
    }
    coalesced
}

async fn execute_report(config: &Config, report: Report) {
    let args = report_args(config, &report);
    let success = run_herdr(&config.binary, args).await;
    if !success {
        tracing::debug!("Herdr report failed; continuing without Herdr integration");
    }
    if let Report::Release { ack: Some(ack), .. } = report {
        let _ = ack.send(());
    }
}

fn report_args(config: &Config, report: &Report) -> Vec<OsString> {
    let mut args = vec![OsString::from("pane")];
    match report {
        Report::State {
            state,
            message,
            seq,
        } => {
            push_strings(&mut args, &["report-agent", &config.pane_id]);
            push_common_args(&mut args, *seq);
            push_strings(&mut args, &["--state", state.as_str()]);
            if let Some(message) = message {
                push_strings(&mut args, &["--message", message]);
            }
        }
        Report::Session {
            session_id,
            resume_argv,
            seq,
        } => {
            push_strings(&mut args, &["report-agent-session", &config.pane_id]);
            push_common_args(&mut args, *seq);
            push_strings(&mut args, &["--agent-session-id", session_id]);
            if let Some(resume_argv) = resume_argv {
                args.push(OsString::from("--"));
                push_string_slice(&mut args, resume_argv);
            }
        }
        Report::Release { seq, .. } => {
            push_strings(&mut args, &["release-agent", &config.pane_id]);
            push_common_args(&mut args, *seq);
        }
    }
    args
}

fn push_common_args(args: &mut Vec<OsString>, seq: u64) {
    push_strings(args, &["--source", SOURCE, "--agent", AGENT, "--seq"]);
    args.push(OsString::from(seq.to_string()));
}

fn push_strings<const N: usize>(args: &mut Vec<OsString>, values: &[&str; N]) {
    args.extend(values.iter().map(OsString::from));
}

fn push_string_slice(args: &mut Vec<OsString>, values: &[String]) {
    args.extend(values.iter().map(OsString::from));
}

async fn run_herdr(binary: &Path, args: Vec<OsString>) -> bool {
    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    match tokio::time::timeout(REPORT_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status.success(),
        Ok(Err(_)) => false,
        Err(_) => {
            let _ = child.kill().await;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn environment_requires_the_complete_pane_context() {
        let values =
            |env: Option<&str>, socket: Option<&str>, binary: Option<&str>, pane: Option<&str>| {
                Config::from_lookup(|name| match name {
                    "HERDR_ENV" => env.map(OsString::from),
                    "HERDR_SOCKET_PATH" => socket.map(OsString::from),
                    "HERDR_BIN_PATH" => binary.map(OsString::from),
                    "HERDR_PANE_ID" => pane.map(OsString::from),
                    _ => None,
                })
            };
        assert!(values(None, Some("sock"), Some("herdr"), Some("w1:p1")).is_none());
        assert!(values(Some("1"), None, Some("herdr"), Some("w1:p1")).is_none());
        assert!(values(Some("1"), Some("sock"), None, Some("w1:p1")).is_none());
        assert!(values(Some("1"), Some("sock"), Some("herdr"), None).is_none());
        let config = values(Some("1"), Some("sock"), Some("herdr"), Some("w1:p1"))
            .expect("complete Herdr environment");
        assert_eq!(config.pane_id, "w1:p1");
    }

    #[test]
    fn state_reports_are_deduplicated_and_session_changes_are_not() {
        let (mut reporter, mut rx) = HerdrReporter::for_test();
        reporter.report_state(HerdrState::Idle, None);
        reporter.report_state(HerdrState::Idle, None);
        reporter.report_state(HerdrState::Blocked, Some("waiting"));
        reporter.report_session(
            "session-a",
            Path::new("/tmp/sessions"),
            Path::new("/tmp/agent"),
            "anthropic",
            "k3",
            Some("high"),
        );
        reporter.report_session(
            "session-a",
            Path::new("/tmp/sessions"),
            Path::new("/tmp/agent"),
            "anthropic",
            "k3",
            Some("high"),
        );
        reporter.report_session(
            "session-a",
            Path::new("/tmp/sessions"),
            Path::new("/tmp/agent"),
            "anthropic",
            "k4",
            Some("high"),
        );
        reporter.report_session(
            "session-b",
            Path::new("/tmp/sessions"),
            Path::new("/tmp/agent"),
            "anthropic",
            "k3",
            Some("high"),
        );

        let first = rx.try_recv().expect("initial state");
        assert!(matches!(
            first,
            Report::State {
                state: HerdrState::Idle,
                ..
            }
        ));
        let second = rx.try_recv().expect("blocked state");
        assert!(matches!(
            second,
            Report::State {
                state: HerdrState::Blocked,
                ..
            }
        ));
        let first_session = rx.try_recv().expect("first session");
        assert!(
            matches!(first_session, Report::Session { ref session_id, .. } if session_id == "session-a")
        );
        let model_change = rx.try_recv().expect("model change");
        assert!(
            matches!(model_change, Report::Session { ref session_id, ref resume_argv, .. } if session_id == "session-a" && resume_argv.as_ref().is_some_and(|argv| argv.contains(&"k4".to_string())))
        );
        let second_session = rx.try_recv().expect("changed session");
        assert!(
            matches!(second_session, Report::Session { ref session_id, .. } if session_id == "session-b")
        );
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn session_report_contains_a_resume_command() {
        let (mut reporter, mut rx) = HerdrReporter::for_test();
        reporter.report_session(
            "abc123",
            Path::new("/tmp/sessions"),
            Path::new("/tmp/agent"),
            "anthropic",
            "k3",
            Some("high"),
        );
        let report = rx.try_recv().expect("session report");
        let config = Config {
            binary: PathBuf::from("herdr"),
            pane_id: "w1:p1".to_string(),
        };
        let args = strings(&report_args(&config, &report));
        assert!(args.starts_with(&[
            "pane".to_string(),
            "report-agent-session".to_string(),
            "w1:p1".to_string(),
        ]));
        let separator = args
            .iter()
            .position(|arg| arg == "--")
            .expect("resume separator");
        assert_eq!(
            &args[separator + 1..],
            &[
                "tack",
                "--agent-dir",
                "/tmp/agent",
                "--session",
                "abc123",
                "--continue",
                "--session-dir",
                "/tmp/sessions",
                "--provider",
                "anthropic",
                "--model",
                "k3",
                "--thinking",
                "high",
            ]
        );
    }

    #[test]
    fn unsafe_resume_arguments_are_omitted_without_losing_session_identity() {
        let resume = |id: &str, directory: &Path| {
            resume_argv(
                id,
                directory,
                Path::new("/tmp/agent"),
                "anthropic",
                "k3",
                None,
            )
        };
        assert!(resume("abc", Path::new("/tmp/sessions")).is_some());
        assert!(resume("abc'def", Path::new("/tmp/sessions")).is_none());
        assert!(resume("abc\ndef", Path::new("/tmp/sessions")).is_none());
        assert!(resume("abc", Path::new("/tmp/it's-here")).is_none());
        assert!(resume(&"x".repeat(MAX_RESUME_BYTES), Path::new("/tmp")).is_none());
        assert!(
            resume_argv(
                "abc",
                Path::new("/tmp/sessions"),
                Path::new("/tmp/it's-agent"),
                "anthropic",
                "k3",
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn blocked_message_is_an_argument_not_shell_text() {
        let config = Config {
            binary: PathBuf::from("herdr"),
            pane_id: "w1:p1".to_string(),
        };
        let args = strings(&report_args(
            &config,
            &Report::State {
                state: HerdrState::Blocked,
                message: Some("waiting; $(not-a-shell)".to_string()),
                seq: 42,
            },
        ));
        assert!(args.contains(&"waiting; $(not-a-shell)".to_string()));
        assert!(args.contains(&"42".to_string()));
    }

    #[test]
    fn queued_state_and_session_reports_keep_only_the_latest_adjacent_value() {
        let reports = coalesce_batch(vec![
            Report::State {
                state: HerdrState::Working,
                message: None,
                seq: 1,
            },
            Report::State {
                state: HerdrState::Blocked,
                message: Some("waiting".to_string()),
                seq: 2,
            },
            Report::Session {
                session_id: "old".to_string(),
                resume_argv: None,
                seq: 3,
            },
            Report::Session {
                session_id: "new".to_string(),
                resume_argv: None,
                seq: 4,
            },
            Report::Release { seq: 5, ack: None },
        ]);
        assert_eq!(reports.len(), 3);
        assert!(matches!(
            reports[0],
            Report::State {
                state: HerdrState::Blocked,
                seq: 2,
                ..
            }
        ));
        assert!(
            matches!(reports[1], Report::Session { ref session_id, seq: 4, .. } if session_id == "new")
        );
        assert!(matches!(reports[2], Report::Release { seq: 5, .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_delivers_state_session_and_release_to_the_cli() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("fake-herdr");
        std::fs::write(
            &binary,
            "#!/bin/sh\n{ printf 'CALL'; for arg in \"$@\"; do printf '\\t%s' \"$arg\"; done; printf '\\n'; } >> \"$(dirname \"$0\")/reports.log\"\n",
        )
        .expect("fake Herdr script");
        let mut permissions = std::fs::metadata(&binary)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&binary, permissions).expect("script permissions");

        let mut reporter = HerdrReporter::from_config(Some(Config {
            binary,
            pane_id: "w1:p1".to_string(),
        }));
        reporter.report_state(HerdrState::Idle, None);
        reporter.report_session(
            "abc123",
            Path::new("/tmp/sessions"),
            Path::new("/tmp/agent"),
            "anthropic",
            "k3",
            Some("high"),
        );
        reporter.release().await;

        let log = std::fs::read_to_string(temp.path().join("reports.log")).expect("report log");
        assert!(log.contains("report-agent\tw1:p1"), "{log}");
        assert!(log.contains("--state\tidle"), "{log}");
        assert!(log.contains("report-agent-session\tw1:p1"), "{log}");
        assert!(
            log.contains("--\ttack\t--agent-dir\t/tmp/agent\t--session\tabc123\t--continue\t--session-dir\t/tmp/sessions\t--provider\tanthropic\t--model\tk3\t--thinking\thigh"),
            "{log}"
        );
        assert!(log.contains("release-agent\tw1:p1"), "{log}");
    }

    #[tokio::test]
    async fn a_missing_herdr_binary_is_best_effort() {
        let binary = Path::new("definitely-not-a-real-herdr-binary");
        assert!(!run_herdr(binary, vec![OsString::from("--version")]).await);
    }
}
