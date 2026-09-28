use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};

use tack_app::{model, print_mode, settings};

#[derive(Parser)]
#[command(
    name = "tack",
    version,
    about = "Rust reimplementation of the pi coding agent (interactive TUI + print + ACP + RPC + serve)"
)]
struct Cli {
    /// Prompt to run (print mode shorthand: `tack -p "task"`)
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    print: Option<String>,

    /// Provider (anthropic | openai)
    #[arg(long)]
    provider: Option<String>,

    /// Model id
    #[arg(long)]
    model: Option<String>,

    /// API key (defaults to provider env vars)
    #[arg(long)]
    api_key: Option<String>,

    /// Continue the most recent session for this directory
    #[arg(short = 'c', long = "continue")]
    continue_session: bool,

    /// Select a session to resume (opens the picker in the TUI)
    #[arg(short = 'r', long = "resume")]
    resume: bool,

    /// Use a specific session file or partial session id
    #[arg(long, value_name = "PATH_OR_ID")]
    session: Option<String>,

    /// Use an exact session id, creating it if missing
    #[arg(long)]
    session_id: Option<String>,

    /// Fork a session file or partial id into a new session
    #[arg(long, value_name = "PATH_OR_ID")]
    fork: Option<String>,

    /// Name the session
    #[arg(short = 'n', long = "name")]
    name: Option<String>,

    /// Don't persist the session
    #[arg(long = "no-session")]
    no_session: bool,

    /// Override the session storage directory
    #[arg(long)]
    session_dir: Option<PathBuf>,

    /// Models for ctrl+p cycling (comma-separated)
    #[arg(long, value_delimiter = ',')]
    models: Vec<String>,

    /// Allowlist of tools (comma-separated)
    #[arg(short = 't', long = "tools", value_delimiter = ',')]
    tools: Vec<String>,

    /// Tools to exclude (comma-separated)
    #[arg(long = "exclude-tools", value_delimiter = ',')]
    exclude_tools: Vec<String>,

    /// Disable all tools
    #[arg(long = "no-tools")]
    no_tools: bool,

    /// Disable the built-in tools (keep MCP tools)
    #[arg(long = "no-builtin-tools")]
    no_builtin_tools: bool,

    /// Disable skills
    #[arg(long = "no-skills")]
    no_skills: bool,

    /// Disable project context files (AGENTS.md etc.)
    #[arg(long = "no-context-files")]
    no_context_files: bool,

    /// Disable prompt templates
    #[arg(long = "no-prompt-templates")]
    no_prompt_templates: bool,

    /// Disable custom JSON themes (built-ins still available)
    #[arg(long = "no-themes")]
    no_themes: bool,

    /// List available models (optionally filtered) and exit
    #[arg(long, value_name = "PATTERN", num_args = 0..=1, default_missing_value = "")]
    list_models: Option<String>,

    /// Print-mode output: text (default), json (JSONL events), or rpc (alias
    /// for the rpc subcommand)
    #[arg(long, value_parser = ["text", "json", "rpc"])]
    mode: Option<String>,

    /// TUI mode override
    #[arg(long, value_parser = ["regular", "fullscreen"])]
    tui_mode: Option<String>,

    /// Theme override (built-in name or theme from the themes dirs)
    #[arg(long = "use-theme")]
    use_theme: Option<String>,

    /// Append text (or a file's contents) to the system prompt (repeatable)
    #[arg(long = "append-system-prompt")]
    append_system_prompt: Vec<String>,

    /// Extra skills directory to load (repeatable)
    #[arg(long = "skill", value_name = "DIR")]
    skill_paths: Vec<PathBuf>,

    /// Additional working directory (repeatable; multi-root sessions)
    #[arg(long = "add-dir", value_name = "DIR")]
    add_dirs: Vec<PathBuf>,

    /// Copy the session file here when the run/session ends
    #[arg(long)]
    export: Option<PathBuf>,

    /// Verbose debug logging (stderr)
    #[arg(long)]
    verbose: bool,

    /// Trust the project (skip the trust prompt for this run)
    #[arg(short = 'a', long = "approve")]
    approve: bool,

    /// Never trust the project for this run
    #[arg(long = "no-approve")]
    no_approve: bool,

    /// Offline mode: block non-LLM network access (share/OAuth login)
    #[arg(long)]
    offline: bool,

    /// Thinking level: off | minimal | low | medium | high | xhigh | max
    #[arg(long)]
    thinking: Option<String>,

    /// Prompt template to expand: "name arg1 arg2" (from prompts/ dirs)
    #[arg(long, value_name = "NAME_ARGS")]
    prompt_template: Option<String>,

    /// Override the system prompt
    #[arg(long)]
    system_prompt: Option<String>,

    /// Positional prompt messages and @file arguments (print mode)
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run as an ACP (Agent Client Protocol) server over stdio
    Acp,
    /// Run as an RPC server (JSONL commands on stdin, events on stdout)
    Rpc,
    /// Expose the agent as an MCP server over stdio (callable by other agents/IDEs)
    McpServe,
    /// Self-check the environment (shell, LSP servers, sandbox, browser, credentials, MCP)
    Doctor {
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
        /// Write a bug-report bundle (tar.gz: doctor report, redacted
        /// settings, crash.log); optionally takes the output path
        #[arg(long, value_name = "PATH", num_args = 0..=1)]
        bundle: Option<Option<PathBuf>>,
    },
    /// View the structured JSONL trace (observability)
    Logs {
        /// Number of events to show
        #[arg(long, default_value = "50")]
        tail: usize,
        /// Minimum level (trace/debug/info/warn/error)
        #[arg(long)]
        level: Option<String>,
        /// Target prefix filter (e.g. tack_app, tack_tools::lsp)
        #[arg(long)]
        target: Option<String>,
        /// Stream new events as they arrive
        #[arg(long)]
        follow: bool,
    },
    /// Run eval tasks (task.json dirs) headlessly and score pass rates
    Eval {
        /// Directory containing task subdirectories with task.json
        dir: PathBuf,
        /// Repeat each task N times (pass rate)
        #[arg(long, default_value = "1")]
        runs: usize,
        /// Only run tasks whose name contains this substring
        #[arg(long)]
        filter: Option<String>,
        /// Write the JSON report here (default: stdout summary only)
        #[arg(long)]
        report: Option<PathBuf>,
        /// Compare against a previous report JSON
        #[arg(long)]
        baseline: Option<PathBuf>,
    },
    /// Host remote sessions over framed CBOR (pi protocol v1)
    Serve {
        /// Listen address: tcp:127.0.0.1:7749 (default), ws:127.0.0.1:7749
        /// (WebSocket + embedded web client at http://addr/), or unix:/path
        /// (unix only)
        #[arg(long, default_value = "tcp:127.0.0.1:7749")]
        listen: String,
        /// Shared auth token required from all clients. Prefer
        /// --auth-token-file or the TACK_REMOTE_TOKEN env var: a literal
        /// value here is visible in shell history and process listings.
        #[arg(long)]
        auth_token: Option<String>,
        /// Read the shared auth token from a file
        #[arg(long, conflicts_with = "auth_token")]
        auth_token_file: Option<PathBuf>,
        /// Start without any auth token even on a non-loopback address
        /// (DANGEROUS: anyone who can reach the port can run commands on
        /// this machine). Loopback/unix listeners never need this.
        #[arg(long)]
        allow_no_auth: bool,
        /// Enable TLS (generates a self-signed pair in the agent dir on first use)
        #[arg(long)]
        tls: bool,
        /// TLS certificate PEM (implies --tls)
        #[arg(long)]
        tls_cert: Option<PathBuf>,
        /// TLS private key PEM (implies --tls)
        #[arg(long)]
        tls_key: Option<PathBuf>,
    },
    /// Store an API key for a provider (~/.tack/agent/auth.json), or run an
    /// OAuth login flow when the provider supports it and no --api-key is given
    Login {
        #[arg(long)]
        provider: String,
        /// API key; omit to read it securely from stdin, or to start an OAuth
        /// login flow for OAuth-capable providers
        #[arg(long)]
        api_key: Option<String>,
        /// Use the device-code OAuth flow (headless environments)
        #[arg(long)]
        device_code: bool,
    },
    /// Remove a provider's stored credential
    Logout {
        #[arg(long)]
        provider: String,
    },
    /// List providers with stored credentials
    AuthStatus,
    /// List every known provider: name, whether credentials are available,
    /// model count, and the env var / login command that enables it
    Providers,
    /// List models (optionally filtered by a substring), grouped by provider
    /// with an auth marker — the discoverable form of --list-models
    Models {
        /// Substring matched against provider/id and model name
        pattern: Option<String>,
    },
    /// Fork a session file into this directory (new session id, keeps history)
    Fork {
        /// Path to the source session .jsonl
        session_file: PathBuf,
    },
    /// Aggregate token usage and estimated cost across all stored sessions
    Stats {
        /// Only include entries on/after this date: YYYY-MM-DD, or Nd (N days ago, e.g. 7d)
        #[arg(long, value_name = "YYYY-MM-DD|Nd")]
        since: Option<String>,
        /// Only include entries on/before this date (YYYY-MM-DD)
        #[arg(long, value_name = "YYYY-MM-DD")]
        until: Option<String>,
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
        /// Only include sessions for this project directory
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
    /// Compact the most recent session for this directory
    Compact,
    /// Probe terminal image protocols (kitty/iterm2/half-block). Hidden.
    #[command(hide = true)]
    DebugImage,
    /// Connect to a `tack serve` endpoint and drive a session interactively
    Client {
        /// tcp:127.0.0.1:7749 (default) or unix:/path (unix only)
        #[arg(long, default_value = "tcp:127.0.0.1:7749")]
        addr: String,
        /// Auth token when the server requires one (prefer the
        /// TACK_REMOTE_TOKEN env var: a literal value lands in shell
        /// history and process listings)
        #[arg(long)]
        auth_token: Option<String>,
        /// Connect over TLS (tcps: or plain tcp: with this flag)
        #[arg(long)]
        tls: bool,
        /// PEM root to trust (e.g. the server's serve-cert.pem)
        #[arg(long)]
        tls_ca: Option<PathBuf>,
        /// Skip TLS verification (testing only)
        #[arg(long)]
        tls_insecure: bool,
        /// Model as provider/id for the new session
        #[arg(long)]
        model: Option<String>,
        /// Thinking level
        #[arg(long)]
        thinking: Option<String>,
    },
    /// Manage tack-ext extensions (install/list/remove)
    Ext {
        #[command(subcommand)]
        command: ExtCommand,
    },
    /// Self-update to the latest GitHub release (tack-v* tags, per-platform
    /// assets). Repo: TACK_UPDATE_REPO or settings updateRepo.
    Update {
        /// Only check for a newer version, don't install
        #[arg(long)]
        check: bool,
        /// Reinstall even when already on the latest version
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum ExtCommand {
    /// Install an extension from a git URL (optionally `#<ref>`-pinned), a
    /// local directory, or a `<plugin>@<marketplace>` spec
    Install {
        /// Git URL (https/ssh, optionally `<url>#<tag|branch|sha>`), local
        /// directory path, or `<plugin>@<marketplace>`
        source: String,
        /// Install into the project's .pi/extensions instead of the user dir
        #[arg(short = 'l', long)]
        local: bool,
    },
    /// Remove an installed extension by name
    Remove {
        name: String,
        /// Remove from the project's .pi/extensions instead of the user dir
        #[arg(short = 'l', long)]
        local: bool,
    },
    /// Enable a previously disabled extension (writes settings
    /// `plugins."<id>".enabled=true`)
    Enable {
        /// Plugin id (`name@source`) or bare name when unambiguous
        name: String,
    },
    /// Disable an extension without uninstalling it (new sessions skip it)
    Disable {
        /// Plugin id (`name@source`) or bare name when unambiguous
        name: String,
    },
    /// Upgrade installed extensions by re-fetching their locked source
    /// (no-op when the resolved commit is unchanged)
    Upgrade {
        /// Plugin id or bare name; omit to upgrade all git installs
        name: Option<String>,
    },
    /// List installed extensions
    List,
    /// Verify installed extensions against the lockfile
    /// (~/.tack/agent/extensions-lock.json): reports ok/changed/
    /// not-a-git-repo/missing per plugin; exits non-zero when any plugin
    /// changed or is missing
    Verify,
    /// Manage extension marketplaces (plugin catalogs)
    Marketplace {
        #[command(subcommand)]
        command: MarketplaceCommand,
    },
    /// Scaffold a new tack-RPC v3 plugin (extension.json + SDK starter).
    /// Dev tooling speaks v3 directly; the session loader switches with
    /// the loader rework (see docs/plugin-roadmap.md)
    New {
        /// Target directory, created by the scaffold
        dir: std::path::PathBuf,
        /// Plugin language: rust | ts | python
        lang: String,
    },
    /// Spawn a v3 plugin and dump its declared capabilities (handshake)
    Inspect {
        /// Extension directory (containing extension.json)
        dir: std::path::PathBuf,
    },
    /// Run a v3 plugin against a scenario file (or stream its logs
    /// until Ctrl-C when no scenario is given)
    Dev {
        /// Extension directory (containing extension.json)
        dir: std::path::PathBuf,
        /// Scenario file (JSON); see crates/tack-app/src/ext_dev.rs
        scenario: Option<std::path::PathBuf>,
    },
    /// Run scenario assertions for a v3 plugin; exits non-zero on
    /// failure (default scenario: `<dir>/plugin.scenario.json`)
    Test {
        /// Extension directory (containing extension.json)
        dir: std::path::PathBuf,
        /// Scenario file (JSON)
        scenario: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum MarketplaceCommand {
    /// Register a marketplace from a JSON file or URL. Signed catalogs
    /// (top-level "signature") require --public-key on first registration;
    /// the key is pinned (TOFU) and re-checked on every install.
    Add {
        name: String,
        /// Local marketplace JSON file or http(s) URL
        source: String,
        /// ed25519 public key (hex) for a signed catalog; pinned for all
        /// future installs from this marketplace
        #[arg(long)]
        public_key: Option<String>,
    },
    /// List registered marketplaces (or one marketplace's plugins)
    List {
        /// Show this marketplace's plugins instead of the marketplace list
        marketplace: Option<String>,
    },
    /// Remove a registered marketplace
    Remove { name: String },
}

/// Install a panic hook that appends crash details to
/// `<agent_dir>/crash.log` and prints a friendly note. Terminal restore on
/// panic is handled separately: `TerminalGuard`'s Drop runs during unwind
/// (release keeps panic=unwind on purpose) and the TUI entry point wraps the
/// event loop in `catch_unwind`.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        // `println!`/`eprintln!` dying on a closed downstream pipe is the
        // classic CLI situation (`tack models | head`), not a bug: exit
        // quietly with success like the consumer expects, and keep the
        // crash log for real panics. Exiting from the hook skips terminal
        // restore, but a broken stdout means the tty is gone anyway.
        if payload.starts_with("failed printing to std") && payload.contains("Broken pipe") {
            std::process::exit(0);
        }
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");
        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        // Honors RUST_BACKTRACE: disabled backtraces render as a short note.
        let backtrace = std::backtrace::Backtrace::capture();
        let entry = format!(
            "[{timestamp}] thread '{thread_name}' panicked at {location}\n{payload}\n{backtrace}\n"
        );
        let log_path = tack_session::default_agent_dir().join("crash.log");
        let mut logged = false;
        if let Some(parent) = log_path.parent()
            && std::fs::create_dir_all(parent).is_ok()
        {
            logged = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .and_then(|mut f| std::io::Write::write_all(&mut f, entry.as_bytes()))
                .is_ok();
        }
        // Best-effort reporting: NEVER use eprintln! here — if stderr is a
        // broken pipe too (dead pty), the macro panics inside the hook and
        // the process aborts (SIGABRT) on the spot.
        use std::io::Write as _;
        let stderr = std::io::stderr();
        let mut err = stderr.lock();
        let _ = writeln!(
            err,
            "\ntack crashed: {payload}\n  at {location} (thread '{thread_name}')"
        );
        if logged {
            let _ = writeln!(err, "Crash details appended to {}", log_path.display());
        }
        let _ = writeln!(err, "This is a bug — please report it with the crash log.");
    }));
}

fn main() -> Result<()> {
    // rustls is provider-less workspace-wide (reqwest rustls-no-provider);
    // install ring before anything builds a TLS client.
    tack_ai::tls::ensure_ring_provider();
    install_panic_hook();
    // Worker threads default to a 2 MiB stack; the agent pump, provider
    // streaming and deep JSON/tool pipelines run on them, and a worker
    // stack overflow aborts the process (SIGABRT) without ever reaching
    // the panic hook — no crash.log, no message. Size them like a normal
    // main-thread stack instead.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()
        .context("failed to build the tokio runtime")?;
    // Windows gives the main thread a 1 MiB stack (vs 8 MiB on
    // Linux/macOS), and `block_on` polls the whole app on it: the async
    // dispatch chain overflows that stack at startup — observed as
    // `tack acp` dying instantly with "thread 'main' has overflowed its
    // stack" on Windows CI, before any panic hook could run (stack
    // overflow is not a Rust panic — no crash.log). Drive the runtime
    // from a dedicated thread with a main-thread-sized stack; the real
    // main thread only joins. The app future is created and polled
    // entirely on that thread, so !Send paths (LocalSet) are unaffected.
    std::thread::Builder::new()
        .name("tack-main".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || runtime.block_on(async_main()))
        .context("failed to spawn the driver thread")?
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

// Early-return style in the command dispatch below is deliberate; the
// lint stayed silent while this body lived inside #[tokio::main].
#[allow(clippy::needless_return)]
async fn async_main() -> Result<()> {
    let cli = Cli::parse();

    // Remove a tack.old.exe remnant from a previous self-update (Windows).
    tack_app::self_update::cleanup_quarantine();

    init_runtime(&cli).await;

    // --approve/--no-approve: session-scoped project trust override.
    apply_trust_override(&cli)?;

    if let Some(pattern) = &cli.list_models {
        list_models(pattern);
        return Ok(());
    }

    let flags = cli_flags(&cli);

    // --mode rpc is an alias for the rpc subcommand.
    if cli.command.is_none() && cli.mode.as_deref() == Some("rpc") {
        return run_rpc(&cli).await;
    }

    match &cli.command {
        Some(Command::Acp) => {
            let overrides = tack_app::acp::agent::AcpOverrides {
                provider: cli.provider.clone(),
                model: cli.model.clone(),
                api_key: cli.api_key.clone(),
                thinking: parse_thinking(&cli)?,
            };
            return tack_app::acp::serve(&overrides).await;
        }
        Some(Command::Rpc) => {
            return run_rpc(&cli).await;
        }
        Some(Command::McpServe) => {
            return cmd_mcp_serve(&cli).await;
        }
        Some(Command::Doctor { json, bundle }) => {
            let code = if let Some(path) = bundle {
                tack_app::doctor::run_bundle(path.clone()).await?
            } else if *json {
                tack_app::doctor::run_json().await?
            } else {
                tack_app::doctor::run().await?
            };
            std::process::exit(code);
        }
        Some(Command::Logs {
            tail,
            level,
            target,
            follow,
        }) => {
            return tack_app::logs::run(tack_app::logs::LogOptions {
                tail: *tail,
                level: level.clone(),
                target: target.clone(),
                follow: *follow,
            })
            .await;
        }
        Some(Command::Eval {
            dir,
            runs,
            filter,
            report,
            baseline,
        }) => {
            return cmd_eval(
                &cli,
                dir,
                *runs,
                filter.as_deref(),
                report.as_ref(),
                baseline.as_ref(),
            )
            .await;
        }
        Some(Command::Serve {
            listen,
            auth_token,
            auth_token_file,
            allow_no_auth,
            tls,
            tls_cert,
            tls_key,
        }) => {
            return cmd_serve(
                &cli,
                listen,
                auth_token,
                auth_token_file,
                *allow_no_auth,
                *tls,
                tls_cert,
                tls_key,
            )
            .await;
        }
        Some(Command::Login {
            provider,
            api_key,
            device_code,
        }) => {
            return cmd_login(provider, api_key, *device_code).await;
        }
        Some(Command::Logout { provider }) => {
            return cmd_logout(provider);
        }
        Some(Command::AuthStatus) => {
            cmd_auth_status();
            return Ok(());
        }
        Some(Command::Providers) => {
            list_providers();
            return Ok(());
        }
        Some(Command::Models { pattern }) => {
            list_models(pattern.as_deref().unwrap_or(""));
            return Ok(());
        }
        Some(Command::Fork { session_file }) => {
            return cmd_fork(session_file);
        }
        Some(Command::Stats {
            since,
            until,
            json,
            dir,
        }) => {
            return cmd_stats(since, until, *json, dir);
        }
        Some(Command::Compact) => {
            let cwd = std::env::current_dir()?;
            let code = tack_app::print_mode::run_compact(cwd).await?;
            std::process::exit(code);
        }
        Some(Command::DebugImage) => {
            return tack_app::debug_image::run();
        }
        Some(Command::Client {
            addr,
            auth_token,
            tls,
            tls_ca,
            tls_insecure,
            model,
            thinking,
        }) => {
            return cmd_client(
                addr,
                auth_token,
                *tls,
                tls_ca,
                *tls_insecure,
                model,
                thinking,
            )
            .await;
        }
        Some(Command::Ext { command }) => {
            return cmd_ext(command).await;
        }
        Some(Command::Update { check, force }) => {
            return cmd_update(*check, *force).await;
        }
        None => {
            return run_default(&cli, flags).await;
        }
    }
}

/// Startup plumbing shared by every subcommand: tracing (stderr + optional
/// structured JSONL export), the cached model catalog, local-provider
/// probing and session-at-rest encryption.
async fn init_runtime(cli: &Cli) {
    // Logs go to stderr only — stdout is reserved for protocol/print output.
    // usvg/fontdb/resvg are silenced: mermaid PNG rendering would otherwise
    // spam font-fallback warnings into the TUI scrollback. Structured JSONL
    // export (settings observability / TACK_TRACE_FILE) layers on top.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(if cli.verbose {
            "debug".to_string()
        } else {
            "warn,usvg=error,resvg=error,fontdb=error".to_string()
        })
    });
    let agent_dir = tack_session::default_agent_dir();
    let settings_raw =
        settings::Settings::load(&std::env::current_dir().unwrap_or_default(), &agent_dir);
    tack_app::observability::init_tracing(filter, &agent_dir, settings_raw.raw());

    // Model catalog: install a previously refreshed catalog (from
    // `/models refresh`) if present — covers print/rpc/acp/serve too.
    if let Some((providers, models)) = tack_app::catalog_refresh::load_cached_override(&agent_dir) {
        tracing::info!(
            providers,
            models,
            "model catalog override loaded from cache"
        );
    }

    // Local providers (Ollama, llama.cpp): probe localhost with a ~500ms
    // timeout and inject discovered models into the catalog. Failures
    // are silent (no local server is the norm); --offline / TACK_OFFLINE
    // skips probing entirely.
    if !cli.offline {
        tack_ai::local_providers::refresh().await;
        // CodeBuddy CLI: discover models via the initialize handshake
        // (silent when the CLI is absent — spawns it once, ~seconds).
        tack_ai::codebuddy::refresh().await;
    }

    // Session-at-rest encryption: resolve the key from the OS keyring
    // (generated on first use) when sessionEncryption is on.
    if settings_raw.session_encryption {
        init_session_encryption();
    }
}

/// Resolve the session-encryption key from the OS keyring (generated on
/// first use) and install it process-wide.
fn init_session_encryption() {
    use base64::Engine as _;
    let entry = keyring::Entry::new("tack", "session-encryption-key");
    let key: Option<[u8; 32]> = entry
        .as_ref()
        .ok()
        .and_then(|e| e.get_password().ok())
        .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok());
    let key = match key {
        Some(key) => Some(key),
        None => {
            let generated: [u8; 32] = rand::random();
            match &entry {
                Ok(e) => {
                    if let Err(err) =
                        e.set_password(&base64::engine::general_purpose::STANDARD.encode(generated))
                    {
                        tracing::warn!(
                            "session encryption: cannot store key in keyring: {err} — sessions stay plaintext"
                        );
                        None
                    } else {
                        Some(generated)
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        "session encryption: keyring unavailable: {err} — sessions stay plaintext"
                    );
                    None
                }
            }
        }
    };
    if let Some(key) = key {
        tack_session::crypto::set_session_key(key);
        tracing::info!("session encryption: active (AES-256-GCM, key from OS keyring)");
    }
}

/// --approve/--no-approve: session-scoped project trust override.
fn apply_trust_override(cli: &Cli) -> Result<()> {
    if cli.approve || cli.no_approve {
        let cwd = std::env::current_dir()?;
        tack_app::project_trust::set_decision(
            &tack_session::default_agent_dir(),
            &cwd,
            cli.approve,
            true,
        );
    }
    Ok(())
}

/// Shared startup plumbing for the model-driven subcommands (rpc,
/// mcp-serve, eval, serve): cwd, agent dir, settings, and the effective
/// model + auth from --provider/--model/--api-key and settings.
struct SubcommandContext {
    cwd: PathBuf,
    agent_dir: PathBuf,
    settings: settings::Settings,
    model: tack_ai::Model,
    auth: std::sync::Arc<dyn tack_ai::oauth::AuthResolver>,
}

fn subcommand_context(cli: &Cli) -> Result<SubcommandContext> {
    let cwd = std::env::current_dir()?;
    let agent_dir = tack_session::default_agent_dir();
    let loaded = settings::Settings::load(&cwd, &agent_dir);
    let provider_name = cli
        .provider
        .as_deref()
        .or(loaded.default_provider.as_deref())
        .unwrap_or("anthropic");
    let model = model::resolve_model(
        provider_name,
        cli.model.as_deref().or(loaded.default_model.as_deref()),
        &agent_dir,
    )
    .map_err(anyhow::Error::msg)?;
    model::enforce_locked(&loaded, &model).map_err(anyhow::Error::msg)?;
    let auth = model::resolve_auth(&model.provider, cli.api_key.clone(), &agent_dir);
    Ok(SubcommandContext {
        cwd,
        agent_dir,
        settings: loaded,
        model,
        auth,
    })
}

/// Parse --thinking into a thinking level.
fn parse_thinking(cli: &Cli) -> Result<Option<tack_ai::ThinkingLevel>> {
    Ok(cli
        .thinking
        .as_deref()
        .map(print_mode::parse_thinking_level)
        .transpose()?
        .flatten())
}

/// `tack rpc` (also the `--mode rpc` alias): JSONL commands on stdin,
/// events on stdout.
async fn run_rpc(cli: &Cli) -> Result<()> {
    let SubcommandContext {
        cwd, model, auth, ..
    } = subcommand_context(cli)?;
    let thinking = parse_thinking(cli)?;
    tack_app::rpc::run_rpc(model, auth, thinking, cwd, cli.continue_session).await
}

/// `tack mcp-serve`: expose the agent as an MCP server over stdio.
async fn cmd_mcp_serve(cli: &Cli) -> Result<()> {
    let SubcommandContext { model, auth, .. } = subcommand_context(cli)?;
    tack_app::mcp_serve::run(model, auth).await
}

/// `tack eval`: run eval tasks headlessly and score pass rates.
async fn cmd_eval(
    cli: &Cli,
    dir: &std::path::Path,
    runs: usize,
    filter: Option<&str>,
    report: Option<&PathBuf>,
    baseline: Option<&PathBuf>,
) -> Result<()> {
    let SubcommandContext {
        settings: loaded,
        model,
        auth,
        ..
    } = subcommand_context(cli)?;
    let provider: std::sync::Arc<dyn tack_ai::Provider> = tack_ai::provider_for(&model)
        .with_context(|| format!("no adapter for api kind {}", model.api))?;
    let provider: std::sync::Arc<dyn tack_ai::Provider> =
        std::sync::Arc::new(tack_ai::retry::RetryingProvider {
            inner: provider,
            policy: loaded.retry.policy(),
            on_retry_scheduled: None,
        });
    let report_data = tack_app::eval::run_eval(dir, runs, filter, &model, provider, auth, &loaded)
        .await
        .map_err(anyhow::Error::msg)?;
    if let Some(baseline_path) = baseline {
        let base_content = std::fs::read_to_string(baseline_path)
            .with_context(|| format!("cannot read baseline {}", baseline_path.display()))?;
        let base_report: tack_app::eval::EvalReport = serde_json::from_str(&base_content)
            .with_context(|| "baseline is not a valid eval report")?;
        print!(
            "{}",
            tack_app::eval::diff_baseline(&report_data, &base_report)
        );
    }
    println!(
        "\nTOTAL: {:.0}% pass rate, ${:.4} total cost",
        report_data.total_pass_rate * 100.0,
        report_data.total_cost
    );
    let json = tack_app::eval::report_json(&report_data);
    match report {
        Some(path) => {
            std::fs::write(path, &json)
                .with_context(|| format!("cannot write report {}", path.display()))?;
            eprintln!("eval: report written to {}", path.display());
        }
        None => println!("{json}"),
    }
    Ok(())
}

/// `tack serve`: host remote sessions over framed CBOR (pi protocol v1).
#[allow(clippy::too_many_arguments)]
async fn cmd_serve(
    cli: &Cli,
    listen: &str,
    auth_token: &Option<String>,
    auth_token_file: &Option<PathBuf>,
    allow_no_auth: bool,
    tls: bool,
    tls_cert: &Option<PathBuf>,
    tls_key: &Option<PathBuf>,
) -> Result<()> {
    let SubcommandContext {
        agent_dir,
        model,
        auth,
        ..
    } = subcommand_context(cli)?;
    if auth_token.is_some() {
        eprintln!(
            "tack serve: WARNING: --auth-token passes the token on the command line, \
             where it lands in shell history and process listings. Prefer \
             --auth-token-file or the TACK_REMOTE_TOKEN env var."
        );
    }
    let auth_token = match (auth_token.clone(), auth_token_file) {
        (Some(token), _) => Some(token),
        (None, Some(path)) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("cannot read auth token file {}", path.display()))?
                .trim()
                .to_string(),
        ),
        (None, None) => std::env::var("TACK_REMOTE_TOKEN").ok(),
    };
    let tls_paths = match (tls, tls_cert, tls_key) {
        (_, Some(cert), Some(key)) => Some((cert.clone(), key.clone())),
        (true, None, None) => Some(tack_app::remote_tls::serve_cert_paths(&agent_dir)?),
        _ => None,
    };
    tack_app::remote::serve(listen, model, auth, auth_token, tls_paths, allow_no_auth).await
}

/// `tack login`: store an API key, or run an OAuth login flow when the
/// provider supports it and no --api-key is given.
async fn cmd_login(provider: &str, api_key: &Option<String>, device_code: bool) -> Result<()> {
    // OAuth path: no explicit key and the provider has an OAuth flow.
    if api_key.is_none() && tack_ai::oauth::oauth_flow(provider).is_some() {
        let agent_dir = tack_session::default_agent_dir();
        return tack_app::oauth_login::run_oauth_login(&agent_dir, provider, device_code).await;
    }
    let key = match api_key {
        Some(k) => k.clone(),
        None => {
            eprint!("API key for {provider}: ");
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf.trim().to_string()
        }
    };
    if key.is_empty() {
        anyhow::bail!("empty API key");
    }
    tack_app::auth::login(&tack_session::default_agent_dir(), provider, &key)?;
    eprintln!("stored credential for {provider}");
    Ok(())
}

/// `tack logout`: remove a provider's stored credential.
fn cmd_logout(provider: &str) -> Result<()> {
    let removed = tack_app::auth::logout(&tack_session::default_agent_dir(), provider)?;
    eprintln!(
        "{}",
        if removed {
            "credential removed"
        } else {
            "no stored credential"
        }
    );
    Ok(())
}

/// `tack auth-status`: list providers with stored credentials.
fn cmd_auth_status() {
    let agent_dir = tack_session::default_agent_dir();
    let providers = tack_app::auth::list(&agent_dir);
    if providers.is_empty() {
        println!("no stored credentials");
    } else {
        for p in providers {
            let kind = match tack_app::auth::get_oauth(&agent_dir, &p) {
                Some(credential) => {
                    let now = tack_ai::now_millis() as i64;
                    if credential.expires == i64::MAX {
                        "oauth (permanent)".to_string()
                    } else if credential.expires <= now {
                        "oauth (expired, will refresh on next use)".to_string()
                    } else {
                        let mins = (credential.expires - now) / 60_000;
                        format!("oauth (access token valid for {mins}m)")
                    }
                }
                None => "api key".to_string(),
            };
            println!("{p} — {kind}");
        }
    }
}

/// `tack fork`: fork a session file into this directory (new session id,
/// keeps history).
fn cmd_fork(session_file: &std::path::Path) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let manager =
        tack_session::SessionManager::fork_from(session_file, &cwd).map_err(anyhow::Error::msg)?;
    println!("{}", manager.session_file().expect("forked file").display());
    Ok(())
}

/// `tack stats`: cross-session usage report (tokens, cost estimate, daily
/// series) over every project session in the agent dir.
fn cmd_stats(
    since: &Option<String>,
    until: &Option<String>,
    json: bool,
    dir: &Option<PathBuf>,
) -> Result<()> {
    tack_app::stats::run(&tack_app::stats::StatsOptions {
        since: since.clone(),
        until: until.clone(),
        json,
        dir: dir.clone(),
        ..Default::default()
    })
}

/// `tack client`: connect to a `tack serve` endpoint and drive a session
/// interactively.
async fn cmd_client(
    addr: &str,
    auth_token: &Option<String>,
    tls: bool,
    tls_ca: &Option<PathBuf>,
    tls_insecure: bool,
    model: &Option<String>,
    thinking: &Option<String>,
) -> Result<()> {
    if auth_token.is_some() {
        eprintln!(
            "tack client: WARNING: --auth-token passes the token on the command line, \
             where it lands in shell history and process listings. Prefer the \
             TACK_REMOTE_TOKEN env var."
        );
    }
    let token = auth_token
        .clone()
        .or_else(|| std::env::var("TACK_REMOTE_TOKEN").ok());
    tack_app::remote_client::run_client(
        addr,
        model.clone(),
        thinking.clone(),
        token,
        tack_app::remote_client::TlsOptions {
            tls,
            ca: tls_ca.clone(),
            insecure: tls_insecure,
        },
    )
    .await
}

/// `tack ext ...`: manage tack-ext extensions (install/list/remove) and
/// marketplaces.
#[cfg(feature = "ext")]
fn resolve_ext_id(
    cwd: &std::path::Path,
    agent_dir: &std::path::Path,
    name: &str,
) -> Result<String> {
    if name.parse::<tack_ext::plugin_id::PluginId>().is_ok() {
        return Ok(name.to_string());
    }
    let matches: Vec<String> = tack_app::extension_host::list_extensions(cwd, agent_dir)
        .into_iter()
        .filter(|info| info.id.starts_with(&format!("{name}@")))
        .map(|info| info.id)
        .collect();
    match matches.len() {
        0 => anyhow::bail!("no extension named {name:?} installed"),
        1 => Ok(matches.into_iter().next().expect("one match")),
        _ => anyhow::bail!(
            "extension name {name:?} is ambiguous across sources; use the full id name@source"
        ),
    }
}

#[cfg(feature = "ext")]
async fn cmd_ext(command: &ExtCommand) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let agent_dir = tack_session::default_agent_dir();
    match command {
        ExtCommand::Install { source, local } => {
            // `<url>#<ref>` pins a tag/branch/commit (git sources only).
            let (source, cli_rev) = tack_app::extension_host::split_source_ref(source);
            // <plugin>@<marketplace> resolves through the catalog.
            let (source, name, rev, marketplace) =
                match tack_app::extension_host::resolve_marketplace_spec(&source, &agent_dir) {
                    Ok(Some(resolved)) => {
                        // An explicit #ref beats the catalog's "rev" pin.
                        let rev = cli_rev.or(resolved.rev);
                        (
                            resolved.source,
                            Some(resolved.plugin),
                            rev,
                            Some(resolved.marketplace),
                        )
                    }
                    Ok(None) => (source, None, cli_rev, None),
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                };
            let dir = tack_app::extension_host::install_extension_named(
                &source,
                &cwd,
                &agent_dir,
                *local,
                name.as_deref(),
                rev.as_deref(),
                marketplace.as_deref(),
            )?;
            println!("installed extension at {}", dir.display());
            Ok(())
        }
        ExtCommand::Remove { name, local } => {
            tack_app::extension_host::remove_extension(name, &cwd, &agent_dir, *local)?;
            println!("removed extension {name}");
            Ok(())
        }
        ExtCommand::List => {
            let extensions = tack_app::extension_host::list_extensions(&cwd, &agent_dir);
            if extensions.is_empty() {
                println!("no extensions installed");
            } else {
                for info in extensions {
                    let state = if info.enabled { "active" } else { "disabled" };
                    let layout = if info.legacy { "legacy" } else { "store" };
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        info.id,
                        state,
                        info.version,
                        layout,
                        info.dir.display()
                    );
                }
            }
            Ok(())
        }
        ExtCommand::Enable { name } => {
            let id = resolve_ext_id(&cwd, &agent_dir, name)?;
            tack_app::extension_host::set_plugin_enabled(&agent_dir, &id, true)?;
            println!("enabled {id}");
            Ok(())
        }
        ExtCommand::Disable { name } => {
            let id = resolve_ext_id(&cwd, &agent_dir, name)?;
            tack_app::extension_host::set_plugin_enabled(&agent_dir, &id, false)?;
            println!("disabled {id}");
            Ok(())
        }
        ExtCommand::Upgrade { name } => {
            let outcomes =
                tack_app::extension_host::upgrade_extensions(&agent_dir, name.as_deref())?;
            if outcomes.is_empty() {
                println!("nothing to upgrade");
            }
            for (id, outcome) in outcomes {
                println!("{id}\t{outcome}");
            }
            Ok(())
        }
        ExtCommand::Verify => {
            use tack_app::extension_host::VerifyStatus;
            let results = tack_app::extension_host::verify_extensions(&agent_dir)?;
            if results.is_empty() {
                println!("no locked extensions to verify");
                return Ok(());
            }
            let mut failures = 0;
            for (name, status) in results {
                match status {
                    VerifyStatus::Ok(commit) => println!("{name}\tok {commit}"),
                    VerifyStatus::Changed { expected, actual } => {
                        failures += 1;
                        println!("{name}\tchanged (locked {expected}, HEAD {actual})");
                    }
                    VerifyStatus::NotAGitRepo => {
                        println!("{name}\tnot-a-git-repo (cannot verify)");
                    }
                    VerifyStatus::Missing => {
                        failures += 1;
                        println!("{name}\tmissing (install directory gone)");
                    }
                }
            }
            if failures > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        ExtCommand::Marketplace { command } => match command {
            MarketplaceCommand::Add {
                name,
                source,
                public_key,
            } => {
                let path = tack_app::extension_host::add_marketplace(
                    name,
                    source,
                    &agent_dir,
                    public_key.as_deref(),
                )
                .await?;
                println!("registered marketplace {name} at {}", path.display());
                Ok(())
            }
            MarketplaceCommand::List { marketplace } => {
                match marketplace {
                    Some(marketplace) => {
                        let plugins =
                            tack_app::extension_host::marketplace_plugins(&agent_dir, marketplace)?;
                        if plugins.is_empty() {
                            println!("marketplace {marketplace} has no plugins");
                        }
                        for (name, source, description) in plugins {
                            match description {
                                Some(description) => {
                                    println!("{name}@{marketplace}\t{source}\t{description}")
                                }
                                None => println!("{name}@{marketplace}\t{source}"),
                            }
                        }
                    }
                    None => {
                        let marketplaces = tack_app::extension_host::list_marketplaces(&agent_dir);
                        if marketplaces.is_empty() {
                            println!("no marketplaces registered");
                        }
                        for (name, path) in marketplaces {
                            println!("{name}\t{}", path.display());
                        }
                    }
                }
                Ok(())
            }
            MarketplaceCommand::Remove { name } => {
                tack_app::extension_host::remove_marketplace(name, &agent_dir)?;
                println!("removed marketplace {name}");
                Ok(())
            }
        },
        ExtCommand::New { dir, lang } => tack_app::ext_dev::cmd_ext_new(dir, lang),
        ExtCommand::Inspect { dir } => tack_app::ext_dev::cmd_ext_inspect(dir).await,
        ExtCommand::Dev { dir, scenario } => {
            tack_app::ext_dev::cmd_ext_dev(dir, scenario.as_deref()).await
        }
        ExtCommand::Test { dir, scenario } => {
            tack_app::ext_dev::cmd_ext_test(dir, scenario.as_deref()).await
        }
    }
}

/// `tack ext …`: extension management (install/remove/list/verify/
/// marketplace). Without the `ext` feature there is nothing to manage.
#[cfg(not(feature = "ext"))]
async fn cmd_ext(command: &ExtCommand) -> Result<()> {
    let _ = command;
    anyhow::bail!("tack was built without extension support (feature `ext` disabled)");
}

/// `tack update`: self-update to the latest GitHub release.
async fn cmd_update(check: bool, force: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let agent_dir = tack_session::default_agent_dir();
    let settings = tack_app::settings::Settings::load(&cwd, &agent_dir);
    let repo = tack_app::self_update::update_repo(settings.update_repo.as_deref());
    match tack_app::self_update::run_update(&repo, check, force).await {
        Ok((outcome, log)) => {
            println!("{log}");
            if matches!(outcome, tack_app::self_update::UpdateOutcome::Updated(_)) {
                println!("restart tack to use the new version");
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

/// No subcommand: interactive TUI when no prompt was given and stdin/stdout
/// are terminals; otherwise print mode.
async fn run_default(cli: &Cli, flags: tack_app::cli_flags::CliFlags) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let agent_dir = tack_session::default_agent_dir();

    // Positional args: @file attachments + prompt messages (TS args).
    let mut file_args: Vec<String> = Vec::new();
    let mut messages: Vec<String> = Vec::new();
    for arg in &cli.args {
        if let Some(file) = arg.strip_prefix('@') {
            file_args.push(file.to_string());
        } else {
            messages.push(arg.clone());
        }
    }
    let positional_prompt = messages.join("\n");

    // No prompt and no subcommand: interactive TUI when stdin/stdout
    // are terminals; otherwise keep requiring a prompt.
    if cli.print.is_none() && cli.prompt_template.is_none() && positional_prompt.is_empty() {
        use std::io::IsTerminal;
        let forced = std::env::var("TACK_TUI_FORCE").is_ok();
        if forced || (std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
            let loaded = settings::Settings::load(&cwd, &agent_dir);
            let provider_name = cli
                .provider
                .as_deref()
                .or(loaded.default_provider.as_deref())
                .unwrap_or("anthropic");
            let model = model::resolve_model(
                provider_name,
                cli.model.as_deref().or(loaded.default_model.as_deref()),
                &agent_dir,
            )
            .map_err(anyhow::Error::msg)?;
            model::enforce_locked(&loaded, &model).map_err(anyhow::Error::msg)?;
            let auth = model::resolve_auth(&model.provider, cli.api_key.clone(), &agent_dir);
            let thinking = parse_thinking(cli)?;
            let code = tack_app::tui::run_tui(tack_app::tui::TuiOptions {
                model,
                auth,
                thinking,
                cwd,
                continue_session: cli.continue_session,
                system_prompt: cli.system_prompt.clone(),
                session_dir: cli.session_dir.clone(),
                flags,
            })
            .await?;
            std::process::exit(code);
        }
        anyhow::bail!("no prompt given; use -p \"task\" or a subcommand (acp)");
    }

    let mut prompt = match (&cli.print, &cli.prompt_template) {
        (Some(p), None) => p.clone(),
        (None, Some(template)) => {
            if flags.no_prompt_templates {
                anyhow::bail!("--no-prompt-templates is set; cannot use --prompt-template");
            }
            // Expand "name arg1 arg2" via the prompts/ directories.
            let parts = tack_app::prompt_templates::parse_command_args(template);
            let Some(name) = parts.first() else {
                anyhow::bail!("--prompt-template requires a template name");
            };
            let templates = tack_app::prompt_templates::load_prompt_templates(
                &cwd,
                &tack_session::default_agent_dir(),
            );
            let Some(t) = templates.iter().find(|t| &t.name == name) else {
                let available: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
                anyhow::bail!(
                    "prompt template {name:?} not found (available: {})",
                    available.join(", ")
                );
            };
            tack_app::prompt_templates::substitute_args(&t.content, &parts[1..])
        }
        (Some(_), Some(_)) => {
            anyhow::bail!("use either -p or --prompt-template, not both");
        }
        (None, None) => String::new(),
    };
    if !positional_prompt.is_empty() {
        if !prompt.is_empty() {
            prompt.push('\n');
        }
        prompt.push_str(&positional_prompt);
    }
    // @file expansion (TS file-processor): text inline, images attached.
    let loaded = settings::Settings::load(&cwd, &tack_session::default_agent_dir());
    let (file_text, file_images) =
        print_mode::process_file_args(&file_args, &cwd, loaded.block_images);
    prompt.push_str(&file_text);
    if prompt.trim().is_empty() && file_images.is_empty() {
        anyhow::bail!("no prompt given; use -p \"task\" or a subcommand (acp)");
    }

    let agent_dir = tack_session::default_agent_dir();
    let provider_name = cli
        .provider
        .as_deref()
        .or(loaded.default_provider.as_deref())
        .unwrap_or("anthropic");
    let model = model::resolve_model(
        provider_name,
        cli.model.as_deref().or(loaded.default_model.as_deref()),
        &agent_dir,
    )
    .map_err(anyhow::Error::msg)?;
    model::enforce_locked(&loaded, &model).map_err(anyhow::Error::msg)?;
    let auth = model::resolve_auth(&model.provider, cli.api_key.clone(), &agent_dir);
    {
        // Warn early when nothing can authenticate a request. CodeBuddy is
        // exempt: auth lives in the CLI session (`codebuddy login`).
        let probe = auth.resolve().await.map_err(anyhow::Error::msg)?;
        let cli_side_auth = model.provider == tack_ai::codebuddy::PROVIDER_ID
            && tack_ai::codebuddy::cli_available();
        if !cli_side_auth && probe.api_key.is_none() && probe.headers.is_empty() {
            let env_hint = tack_ai::providers::builtin_provider(&model.provider)
                .and_then(|d| d.env_keys.first().copied())
                .unwrap_or("the provider's API key env var");
            anyhow::bail!(
                "no API key for provider {} (set {} or pass --api-key); \
                 see `tack providers` for all providers and `tack login --provider {}` to log in",
                model.provider,
                env_hint,
                model.provider
            );
        }
    }

    let code = print_mode::run_print(print_mode::PrintOptions {
        prompt,
        model,
        auth,
        continue_session: cli.continue_session,
        session_dir: cli.session_dir.clone(),
        cwd,
        system_prompt: cli.system_prompt.clone(),
        thinking: parse_thinking(cli)?,
        file_images,
        flags,
    })
    .await?;
    std::process::exit(code);
}

/// Map the clap CLI onto the shared flag bundle.
fn cli_flags(cli: &Cli) -> tack_app::cli_flags::CliFlags {
    tack_app::cli_flags::CliFlags {
        resume: cli.resume,
        session: cli.session.clone(),
        session_id: cli.session_id.clone(),
        fork: cli.fork.clone(),
        name: cli.name.clone(),
        no_session: cli.no_session,
        models: cli.models.clone(),
        tools: cli.tools.clone(),
        exclude_tools: cli.exclude_tools.clone(),
        no_tools: cli.no_tools,
        no_builtin_tools: cli.no_builtin_tools,
        no_skills: cli.no_skills,
        no_context_files: cli.no_context_files,
        no_prompt_templates: cli.no_prompt_templates,
        no_themes: cli.no_themes,
        mode_json: cli.mode.as_deref() == Some("json"),
        tui_mode: cli.tui_mode.clone(),
        use_theme: cli.use_theme.clone(),
        append_system_prompt: cli.append_system_prompt.clone(),
        skill_paths: cli.skill_paths.clone(),
        add_dirs: cli.add_dirs.clone(),
        export: cli.export.clone(),
        offline: cli.offline,
        approve: if cli.approve {
            Some(true)
        } else if cli.no_approve {
            Some(false)
        } else {
            None
        },
    }
}

/// `tack providers` / `--list-providers`: every provider with auth status,
/// model count, and how to enable it — the first-run discovery command.
fn list_providers() {
    let agent_dir = tack_session::default_agent_dir();
    let custom = tack_ai::providers::load_custom_providers(&agent_dir);
    println!("Providers — ✓ = credentials available, so the provider works right away");
    println!();
    let mut ready = 0;
    for def in tack_ai::providers::BUILTIN_PROVIDERS {
        if tack_ai::local_providers::is_local_provider(def.id) {
            // Zero-config local servers: no credentials, readiness = server
            // detected by the startup probe.
            let models = tack_ai::providers::builtin_models(def.id);
            let (mark, note) = match tack_ai::local_providers::status(def.id) {
                Some(s) if s.running => ("✓", format!("(running at {} — no login needed)", s.host)),
                Some(_) => (
                    " ",
                    format!(
                        "(not detected — start with: {})",
                        tack_ai::local_providers::start_hint(def.id)
                    ),
                ),
                None if tack_ai::local_providers::offline_mode() => {
                    (" ", "(offline — probing skipped)".to_string())
                }
                None => (" ", "(not probed)".to_string()),
            };
            ready += usize::from(mark == "✓");
            println!(
                "{mark} {:<24} {:<32} {:>3} models  {note}",
                def.id,
                def.name,
                models.len()
            );
            continue;
        }
        if def.id == tack_ai::codebuddy::PROVIDER_ID {
            // CodeBuddy: auth lives in the CLI (`codebuddy login`), so
            // readiness = CLI binary detected.
            let models = tack_ai::providers::builtin_models(def.id);
            let (mark, note) = if tack_ai::codebuddy::cli_available() {
                (
                    "✓",
                    "(codebuddy CLI detected — auth via `codebuddy login`)".to_string(),
                )
            } else {
                (
                    " ",
                    "(CLI not found — install codebuddy or set CODEBUDDY_PATH)".to_string(),
                )
            };
            ready += usize::from(mark == "✓");
            println!(
                "{mark} {:<24} {:<32} {:>3} models  {note}",
                def.id,
                def.name,
                models.len()
            );
            continue;
        }
        let has_auth = model::provider_has_auth(&agent_dir, def.id);
        ready += usize::from(has_auth);
        let models = tack_ai::providers::builtin_models(def.id);
        let mark = if has_auth { "✓" } else { " " };
        let enable = if def.env_keys.is_empty() {
            String::new()
        } else if has_auth {
            format!("({})", def.env_keys[0])
        } else {
            format!(
                "(set {} or: tack login --provider {})",
                def.env_keys[0], def.id
            )
        };
        println!(
            "{mark} {:<24} {:<32} {:>3} models  {enable}",
            def.id,
            def.name,
            models.len()
        );
    }
    for cp in &custom {
        ready += usize::from(custom_provider_ready(&agent_dir, &cp.id));
        let mark = if custom_provider_ready(&agent_dir, &cp.id) {
            "✓"
        } else {
            " "
        };
        println!(
            "{mark} {:<24} {:<32} {:>3} models  (custom, models.json)",
            cp.id,
            "",
            cp.models.len()
        );
    }
    println!();
    println!(
        "{ready} provider(s) ready. Pick a model with `tack --provider <id> --model <model-id> \"prompt\"`,\n\
         list models with `tack models [pattern]`, or log in interactively with `tack login --provider <id>`."
    );
}

/// `tack models [pattern]` / `--list-models [pattern]`: catalog grouped by
/// provider, with an auth marker on each provider header.
fn list_models(pattern: &str) {
    let agent_dir = tack_session::default_agent_dir();
    let pattern = pattern.to_lowercase();
    let custom = tack_ai::providers::load_custom_providers(&agent_dir);
    for def in tack_ai::providers::BUILTIN_PROVIDERS {
        let catalog = tack_ai::providers::builtin_models(def.id);
        let models = catalog
            .iter()
            .filter(|m| {
                let full = format!("{}/{}", def.id, m.id);
                pattern.is_empty()
                    || full.to_lowercase().contains(&pattern)
                    || m.name.to_lowercase().contains(&pattern)
            })
            .collect::<Vec<_>>();
        if models.is_empty() {
            continue;
        }
        let mark = if model::provider_has_auth(&agent_dir, def.id) {
            "✓"
        } else {
            " "
        };
        println!("{mark} {} — {}", def.id, def.name);
        for m in models {
            println!("    {}/{}\t{}", def.id, m.id, m.name);
        }
    }
    for cp in &custom {
        let models = cp
            .models
            .iter()
            .filter(|m| {
                let full = format!("{}/{}", cp.id, m.id);
                pattern.is_empty()
                    || full.to_lowercase().contains(&pattern)
                    || m.name.to_lowercase().contains(&pattern)
            })
            .collect::<Vec<_>>();
        if models.is_empty() {
            continue;
        }
        let mark = if custom_provider_ready(&agent_dir, &cp.id) {
            "✓"
        } else {
            " "
        };
        println!("{mark} {} — custom (models.json)", cp.id);
        for m in models {
            println!("    {}/{}\t{}", cp.id, m.id, m.name);
        }
    }
}

/// Auth marker for a custom (models.json) provider: a stored credential
/// (auth.json, incl. `tack login --provider <custom-id>`) or an inline /
/// runtime apiKey — the same predicate the model picker uses, so the ✓
/// marker never disagrees with what actually authenticates.
fn custom_provider_ready(agent_dir: &std::path::Path, provider_id: &str) -> bool {
    model::provider_has_auth(agent_dir, provider_id)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod cli_tests {
    /// `tack stats`: flag parsing (YYYY-MM-DD / Nd / --json / --dir).
    #[test]
    fn stats_subcommand_parses_flags() {
        use clap::Parser;
        let cli = super::Cli::try_parse_from(["tack", "stats"]).unwrap();
        let Some(super::Command::Stats {
            since,
            until,
            json,
            dir,
        }) = cli.command
        else {
            panic!("expected stats subcommand");
        };
        assert!(since.is_none() && until.is_none() && !json && dir.is_none());

        let cli = super::Cli::try_parse_from([
            "tack",
            "stats",
            "--since",
            "7d",
            "--until",
            "2026-08-31",
            "--json",
            "--dir",
            "/work/proj",
        ])
        .unwrap();
        let Some(super::Command::Stats {
            since,
            until,
            json,
            dir,
        }) = cli.command
        else {
            panic!("expected stats subcommand");
        };
        assert_eq!(since.as_deref(), Some("7d"));
        assert_eq!(until.as_deref(), Some("2026-08-31"));
        assert!(json);
        assert_eq!(dir, Some(std::path::PathBuf::from("/work/proj")));

        // Unknown flags are rejected.
        assert!(super::Cli::try_parse_from(["tack", "stats", "--bogus"]).is_err());
    }

    /// The stats date filters accept YYYY-MM-DD and Nd, and reject a
    /// reversed range.
    #[test]
    fn stats_date_filters_validate() {
        let today = chrono::NaiveDate::parse_from_str("2026-08-31", "%Y-%m-%d").unwrap();
        assert!(tack_app::stats::parse_since("7d", today).is_ok());
        assert!(tack_app::stats::parse_since("2026-08-01", today).is_ok());
        assert!(tack_app::stats::parse_since("last-week", today).is_err());
        assert!(tack_app::stats::parse_until("2026-08-31").is_ok());
        assert!(tack_app::stats::parse_until("7d").is_err());
    }
    /// `tack providers` / `tack models` must mark a custom provider as
    /// ready when its credential lives in auth.json (not only when
    /// models.json carries an inline apiKey).
    #[test]
    fn custom_provider_marker_honors_stored_credentials() {
        let dir = tempfile::tempdir().unwrap();
        // Pin file storage: login() would otherwise hit the real OS keyring.
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"credentialStore": "file"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("models.json"),
            r#"{"providers":{"acme":{"baseUrl":"https://acme.example.com/v1","api":"openai-completions","models":[{"id":"m1"}]}}}"#,
        )
        .unwrap();
        assert!(!super::custom_provider_ready(dir.path(), "acme"));
        tack_app::auth::login(dir.path(), "acme", "sk-test").unwrap();
        assert!(super::custom_provider_ready(dir.path(), "acme"));
    }
}
