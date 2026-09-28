//! tack-ext-wasm: sandboxed WASM carrier for tack-RPC v3 plugins.
//!
//! A plugin is a WASI preview1 core module (`.wasm` or `.wat`) speaking the
//! exact same JSON-RPC 2.0 / NDJSON protocol as process-carrier plugins over its
//! WASI stdin/stdout. The host side wires the module's stdio to in-memory
//! tokio duplex pipes and hands the other ends to tack-ext's transport-
//! agnostic [`JsonRpcPeer`], so handshake, request/response matching,
//! timeouts, and fail-fast semantics are shared verbatim with the process
//! carrier — only the "spawn a child process" step is replaced by
//! "instantiate a wasm module".
//!
//! Sandbox posture (see `docs/extensions-v2.md`):
//! - **No ambient capabilities**: the WASI context is built with no args,
//!   no env, no preopened directories, no network. The only capabilities
//!   the guest has are stdin/stdout (the protocol pipe) and a sink for
//!   stderr (forwarded to the host log, like v1).
//! - **Fuel metering** (`max_fuel`): CPU budget per request, replenished
//!   only on protocol-pipe progress (request bytes read from stdin,
//!   response bytes written to stdout); exhaustion traps the guest.
//! - **Epoch interruption** (`max_execution`): wall-clock cap (default
//!   10 minutes) enforced by an engine epoch ticker; deadline expiry
//!   traps the guest even if it never yields to fuel accounting.
//! - **Memory cap** (`max_memory_bytes`): linear-memory limit via a
//!   `ResourceLimiter`, checked at instantiation and on `memory.grow`.
//!
//! Capability escape hatches (host functions for `exec`, fs preopens, ...)
//! are a deliberate v2.x decision and NOT granted by default. When a host
//! *does* want to grant a plugin specific capabilities, it passes an
//! explicit [`WasmCapabilities`] to [`WasmCarrier::spawn_with_capabilities`]
//! — every grant is opt-in and [`WasmCapabilities::default`] is exactly the
//! full sandbox described above.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

pub mod component;

use tack_ext::v3::{HostClient, JsonRpcPeer, PeerHandler};
use tokio::io::{AsyncRead, AsyncWrite, BufReader, ReadBuf};
use wasmtime::{CallHook, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::cli::{AsyncStdinStream, AsyncStdoutStream};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::{FsPerms, WasiCtxBuilder};

/// Engine epoch tick used to enforce [`WasmLimits::max_execution`].
const EPOCH_TICK: Duration = Duration::from_millis(10);

/// In-memory pipe capacity between host and guest (each direction).
const PIPE_BUFFER: usize = 64 * 1024;

/// Per-write budget for the guest's stdout/stderr async streams (bytes the
/// WASI stream may flush per readiness check).
const WRITE_BUDGET: usize = 1024 * 1024;

/// Resource limits applied to one plugin instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WasmLimits {
    /// Fuel budget for the guest, replenished to this value every time
    /// the guest moves bytes on the PROTOCOL pipes (reading request
    /// bytes from stdin, writing response bytes to stdout). The budget
    /// therefore bounds the work a plugin does between protocol
    /// interactions — i.e. per request — rather than across its whole
    /// lifetime. Fuel is consumed by executed wasm instructions;
    /// exhaustion traps the guest (CPU DoS protection). Metadata WASI
    /// calls (`clock_time_get`, `fd_fdstat_get`, ...) deliberately do
    /// NOT replenish: a guest looping {{ pure compute; clock_time_get() }}
    /// must not be able to refuel forever.
    pub max_fuel: u64,
    /// Linear memory cap in bytes, enforced at instantiation and on
    /// `memory.grow` via a store `ResourceLimiter`.
    pub max_memory_bytes: usize,
    /// Wall-clock hang watchdog for the guest, enforced via engine epoch
    /// interruption (checked at loop back-edges and calls, so it also
    /// catches tight loops between host calls). The deadline is RE-ARMED
    /// to a fresh window every time the guest makes protocol I/O
    /// progress, so this bounds each no-progress stretch rather than the
    /// guest's total lifetime: a well-behaved long-lived plugin is never
    /// trapped, while a guest that goes silent (deadlock, infinite loop
    /// off the protocol pipes) is. Defaults to [`DEFAULT_MAX_EXECUTION`];
    /// `None` opts out explicitly (per-request protocol timeouts still
    /// apply on the peer side).
    pub max_execution: Option<Duration>,
}

/// Host-side ceiling for [`WasmLimits::max_fuel`]: a plugin manifest may
/// declare its own limits, but a plugin must not be able to weaken its
/// own sandbox. 10x the default per-request budget.
pub const HOST_MAX_FUEL: u64 = 10_000_000_000;
/// Host-side ceiling for [`WasmLimits::max_memory_bytes`] (1 GiB).
pub const HOST_MAX_MEMORY_BYTES: usize = 1024 * 1024 * 1024;
/// Host-side ceiling for [`WasmLimits::max_execution`] (1 hour). `None`
/// (an explicit opt-out of the wall-clock cap) is left untouched.
pub const HOST_MAX_EXECUTION: Duration = Duration::from_secs(3600);

/// Default wall-clock window without protocol I/O progress after which
/// the guest is trapped: 10 minutes. A guest gets ONE `_start` for its
/// whole life; the deadline re-arms on every protocol-pipe byte moved
/// (see the call hook at instantiation), so this is a hang watchdog, not
/// a lifetime cap. Without a default, a guest that keeps its fuel topped
/// up via protocol I/O would run forever with no backstop.
///
/// Caveat: blocking NON-protocol WASI calls (a long `poll_oneoff` sleep,
/// preopen file reads) do not re-arm the deadline — a guest that sleeps
/// longer than this window without touching the protocol pipes is
/// trapped on resume. Such guests must opt out per-plugin via
/// [`WasmLimits::max_execution`] (`None`).
pub const DEFAULT_MAX_EXECUTION: Duration = Duration::from_secs(600);

impl WasmLimits {
    /// Clamp to the host-side ceilings: callers pass plugin-manifest
    /// limits straight through, and a plugin must not be able to raise
    /// its own sandbox budget by declaring a huge one.
    pub fn clamped(&self) -> Self {
        WasmLimits {
            max_fuel: self.max_fuel.min(HOST_MAX_FUEL),
            max_memory_bytes: self.max_memory_bytes.min(HOST_MAX_MEMORY_BYTES),
            max_execution: self.max_execution.map(|d| d.min(HOST_MAX_EXECUTION)),
        }
    }
}

impl Default for WasmLimits {
    fn default() -> Self {
        WasmLimits {
            max_fuel: 1_000_000_000,
            max_memory_bytes: 256 * 1024 * 1024,
            max_execution: Some(DEFAULT_MAX_EXECUTION),
        }
    }
}

/// Sandbox caps that are NOT per-plugin configurable: module byte size,
/// tables, table elements, instances, memories. Set once per carrier
/// ([`WasmCarrier::with_sandbox_caps`]); the defaults are conservative.
/// (A separate struct because `WasmLimits` is constructed with struct
/// literals downstream — adding fields there would break every host.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WasmSandboxCaps {
    /// Cap on the module's byte size (binary `.wasm` or `.wat` text),
    /// checked before compilation: a hostile plugin must not be able to
    /// burn unbounded compile time/memory with a gigantic module.
    pub max_module_bytes: usize,
    /// Max number of tables per instance.
    pub max_tables: usize,
    /// Max elements any one table may hold (funcref tables are an
    /// allocation vector just like linear memory).
    pub max_table_elements: usize,
    /// Max number of instances per store.
    pub max_instances: usize,
    /// Max number of linear memories per instance (multi-memory).
    pub max_memories: usize,
}

impl Default for WasmSandboxCaps {
    fn default() -> Self {
        WasmSandboxCaps {
            max_module_bytes: 64 * 1024 * 1024,
            max_tables: 64,
            max_table_elements: 1_000_000,
            max_instances: 8,
            max_memories: 4,
        }
    }
}

/// Access mode of a single [`PreopenGrant`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreopenAccess {
    /// Guest may open files under the preopen for reading only. Mapped to
    /// `wasmtime_wasi::FsPerms::ReadOnly`.
    ReadOnly,
    /// Guest may read, create, modify, and delete files under the preopen.
    /// Mapped to `wasmtime_wasi::FsPerms::ReadWrite`.
    ReadWrite,
}

/// Grants the guest access to one host directory, mounted at a guest path.
///
/// Each grant becomes one WASI preopened directory (fd 3, 4, ... in grant
/// order for WASI p1 guests). Path resolution is capability-scoped by the
/// runtime: paths escaping `host_path` (e.g. `../..`) are rejected with
/// `ENOTCAPABLE` by wasmtime-wasi regardless of the access mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreopenGrant {
    /// Directory on the host filesystem. Must exist at spawn time.
    pub host_path: PathBuf,
    /// Path the guest sees the directory as (its `fd_prestat_dir_name`).
    /// Conventionally absolute (e.g. `"/data"`); the exact string is only
    /// cosmetic for p1 guests, which address preopens by fd.
    pub guest_path: String,
    /// Read-only or read-write access.
    pub access: PreopenAccess,
}

/// Network capability flags (TCP, UDP, DNS name lookup).
///
/// **These flags are forward-looking and currently inert for the guests
/// this carrier runs.** They map to wasmtime-wasi 48's
/// `WasiCtxBuilder::allow_tcp` / `allow_udp` / `allow_ip_name_lookup`,
/// which configure the WASI **p2** sockets context. The p1 ABI
/// (`wasi_snapshot_preview1`) that this carrier links has no working
/// socket entry points — wasmtime-wasi 48's p1 `sock_*` functions are
/// unimplemented stubs — so a WASI p1 guest cannot open sockets whether
/// or not these are set. The grants are recorded on the context so that
/// future p2/component guests (or future host-provided socket functions)
/// can honor them without an API change here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetworkGrants {
    /// Allow `wasi:sockets/tcp` usage (p2 only; see struct docs).
    pub allow_tcp: bool,
    /// Allow `wasi:sockets/udp` usage (p2 only; see struct docs).
    pub allow_udp: bool,
    /// Allow `wasi:sockets/ip-name-lookup` (DNS) usage (p2 only; see
    /// struct docs).
    pub allow_dns: bool,
}

/// Explicit capability grants for one plugin instance.
///
/// The default is the full sandbox: **every field empty/false** — no
/// preopened directories, no environment variables, no argv, no network
/// — which is exactly the context [`WasmCarrier::spawn`] builds. Grants
/// are additive and opt-in; pass them via
/// [`WasmCarrier::spawn_with_capabilities`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WasmCapabilities {
    /// Host directories preopened into the guest (fd 3, 4, ... in order).
    pub preopens: Vec<PreopenGrant>,
    /// Environment variables visible to the guest via `environ_get`.
    pub env: Vec<(String, String)>,
    /// argv visible to the guest via `args_get` (`argv[0]` conventionally
    /// the program name).
    pub args: Vec<String>,
    /// Network grants. Inert for WASI p1 guests today — see
    /// [`NetworkGrants`].
    pub network: NetworkGrants,
}

/// Per-store state: the WASI p1 context plus the memory limiter.
struct PluginState {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
}

/// Wraps the guest's stdin pipe and counts PROTOCOL bytes the guest
/// actually consumed (see the fuel-replenishment comment at the call
/// hook: only real protocol progress refuels).
struct CountingReader<R> {
    inner: R,
    progress: Arc<AtomicU64>,
}

impl<R: AsyncRead + Unpin> AsyncRead for CountingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let read = buf.filled().len() - before;
                if read > 0 {
                    self.progress.fetch_add(read as u64, Ordering::Relaxed);
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

/// Wraps the guest's stdout pipe and counts PROTOCOL bytes the guest
/// actually produced (stderr is a log sink, not protocol progress, and
/// is deliberately not counted).
struct CountingWriter<W> {
    inner: W,
    progress: Arc<AtomicU64>,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for CountingWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(written)) => {
                if written > 0 {
                    self.progress.fetch_add(written as u64, Ordering::Relaxed);
                }
                Poll::Ready(Ok(written))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// On-demand wake source for the epoch ticker. The ticker only needs to
/// run while at least one guest with a `max_execution` wall-clock
/// deadline is in flight — epoch increments are a no-op for stores
/// without a deadline, so ticking with no deadline'd guest is pure
/// wake-up churn (100 task wakes/sec for the carrier's lifetime).
/// `acquire`/`release` bracket a deadline'd guest's run; the ticker
/// sleeps on `notified()` while `active()` is false.
struct EpochDemand {
    in_flight: AtomicUsize,
    wake: tokio::sync::Notify,
}

impl EpochDemand {
    /// Register a deadline'd guest starting; wakes the ticker if it was
    /// sleeping.
    fn acquire(&self) {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        self.wake.notify_one();
    }

    /// Register a deadline'd guest finishing (run, trap, or abort).
    fn release(&self) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }

    fn active(&self) -> bool {
        self.in_flight.load(Ordering::Acquire) != 0
    }

    async fn notified(&self) {
        self.wake.notified().await;
    }
}

/// RAII hold on one in-flight deadline'd guest. Dropped when the guest
/// task finishes OR is aborted (aborting a tokio task drops its future
/// and everything it captured), so the ticker cannot leak awake.
struct EpochDemandGuard(Arc<EpochDemand>);

impl Drop for EpochDemandGuard {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Shared wasmtime engine for WASM-carrier plugins. One engine per process
/// (compilation is cached by the engine); each plugin gets its own store.
pub struct WasmCarrier {
    engine: Engine,
    caps: WasmSandboxCaps,
    /// Drives epoch deadlines. Aborted when the carrier is dropped.
    /// Sleeps unless [`EpochDemand`] reports a deadline'd guest in flight.
    ticker: tokio::task::JoinHandle<()>,
    epoch_demand: Arc<EpochDemand>,
}

impl std::fmt::Debug for WasmCarrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmCarrier").finish_non_exhaustive()
    }
}

impl Drop for WasmCarrier {
    fn drop(&mut self) {
        self.ticker.abort();
    }
}

impl WasmCarrier {
    /// Build an engine with async support, fuel metering, and epoch
    /// interruption enabled (limits are per-instance, see [`WasmLimits`]).
    pub fn new() -> Result<Self, String> {
        Self::with_sandbox_caps(WasmSandboxCaps::default())
    }

    /// [`WasmCarrier::new`] with explicit sandbox caps.
    pub fn with_sandbox_caps(caps: WasmSandboxCaps) -> Result<Self, String> {
        let mut config = Config::new();
        // Async is always on in wasmtime 48 (`async_support` is a no-op);
        // instantiate_async/call_async are available unconditionally.
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(|e| format!("wasmtime engine: {e}"))?;
        let ticker_engine = engine.clone();
        let epoch_demand = Arc::new(EpochDemand {
            in_flight: AtomicUsize::new(0),
            wake: tokio::sync::Notify::new(),
        });
        let ticker_demand = epoch_demand.clone();
        let ticker = tokio::spawn(async move {
            loop {
                // Park until a guest with an execution deadline is in
                // flight; `Notify` stores one permit, so an `acquire`
                // racing the `active()` check is not lost.
                while !ticker_demand.active() {
                    ticker_demand.notified().await;
                }
                tokio::time::sleep(EPOCH_TICK).await;
                ticker_engine.increment_epoch();
            }
        });
        Ok(WasmCarrier {
            engine,
            caps,
            ticker,
            epoch_demand,
        })
    }

    /// Instantiate a WASI p1 plugin module (`.wasm` binary or `.wat` text)
    /// with the given limits and wire its stdio to a fresh [`JsonRpcPeer`].
    ///
    /// The guest runs with the full default sandbox (no preopens, env,
    /// args, or network). This is
    /// `spawn_with_capabilities(wasm, limits, services, &WasmCapabilities::default())`.
    pub async fn spawn(
        &self,
        wasm: &[u8],
        limits: &WasmLimits,
        services: Arc<dyn PeerHandler>,
    ) -> Result<WasmPlugin, String> {
        self.spawn_with_capabilities(wasm, limits, services, &WasmCapabilities::default())
            .await
    }

    /// [`WasmCarrier::spawn`] with explicit capability grants.
    ///
    /// `capabilities` is applied on top of the base WASI context (stdio
    /// pipes only); anything not granted stays sandboxed. Preopen grants
    /// are opened on the host at spawn time — a missing/inaccessible
    /// `host_path` fails the spawn before the module is instantiated.
    pub async fn spawn_with_capabilities(
        &self,
        wasm: &[u8],
        limits: &WasmLimits,
        services: Arc<dyn PeerHandler>,
        capabilities: &WasmCapabilities,
    ) -> Result<WasmPlugin, String> {
        if wasm.len() > self.caps.max_module_bytes {
            return Err(format!(
                "plugin module is {} bytes, exceeding the {} byte cap",
                wasm.len(),
                self.caps.max_module_bytes
            ));
        }
        // The limits come from the plugin's own manifest downstream; a
        // plugin must not be able to weaken its own sandbox, so clamp to
        // the host-side ceilings no matter what was declared.
        let clamped = limits.clamped();
        if clamped != *limits {
            tracing::warn!(
                declared = ?limits,
                clamped = ?clamped,
                "plugin declared limits above the host ceilings; clamped"
            );
        }
        let limits = &clamped;
        let module = Module::new(&self.engine, wasm)
            .map_err(|e| format!("failed to compile plugin module: {e}"))?;

        // Host writes plugin stdin; host reads plugin stdout/stderr.
        let (guest_stdin, host_stdin) = tokio::io::duplex(PIPE_BUFFER);
        let (host_stdout, guest_stdout) = tokio::io::duplex(PIPE_BUFFER);
        let (host_stderr, guest_stderr) = tokio::io::duplex(PIPE_BUFFER);

        // Base WASI context: only the protocol pipes exist. Everything
        // else is an explicit opt-in grant from `capabilities`. The
        // stdin/stdout pipes are wrapped to count protocol byte movement
        // — the fuel-replenishment hook below refuels ONLY on that
        // progress, so metadata WASI calls (clock_time_get,
        // fd_fdstat_get, ...) cannot keep a spinning guest alive.
        let io_progress = Arc::new(AtomicU64::new(0));
        let mut builder = WasiCtxBuilder::new();
        builder
            .stdin(AsyncStdinStream::new(CountingReader {
                inner: guest_stdin,
                progress: io_progress.clone(),
            }))
            .stdout(AsyncStdoutStream::new(
                WRITE_BUDGET,
                CountingWriter {
                    inner: guest_stdout,
                    progress: io_progress.clone(),
                },
            ))
            .stderr(AsyncStdoutStream::new(WRITE_BUDGET, guest_stderr));
        for grant in &capabilities.preopens {
            let perms = match grant.access {
                PreopenAccess::ReadOnly => FsPerms::ReadOnly,
                PreopenAccess::ReadWrite => FsPerms::ReadWrite,
            };
            builder
                .preopened_dir(&grant.host_path, &grant.guest_path, perms)
                .map_err(|e| {
                    format!(
                        "failed to preopen host dir {} as guest path {}: {e}",
                        grant.host_path.display(),
                        grant.guest_path
                    )
                })?;
        }
        for (key, value) in &capabilities.env {
            builder.env(key, value);
        }
        if !capabilities.args.is_empty() {
            builder.args(&capabilities.args);
        }
        // Forward-looking ctx configuration: WASI p1 guests have no socket
        // entry points, so these flags are inert today (see NetworkGrants).
        builder
            .allow_tcp(capabilities.network.allow_tcp)
            .allow_udp(capabilities.network.allow_udp)
            .allow_ip_name_lookup(capabilities.network.allow_dns);
        let wasi = builder.build_p1();

        let state = PluginState {
            wasi,
            limits: StoreLimitsBuilder::new()
                .memory_size(limits.max_memory_bytes)
                .tables(self.caps.max_tables)
                .table_elements(self.caps.max_table_elements)
                .instances(self.caps.max_instances)
                .memories(self.caps.max_memories)
                .build(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(limits.max_fuel)
            .map_err(|e| format!("failed to set fuel: {e}"))?;
        // Replenish the fuel budget every time the guest moves bytes on
        // the PROTOCOL pipes (consuming request bytes, producing
        // response bytes). The guest's whole life is one `_start` call,
        // so without replenishment the budget would be a one-shot
        // lifetime allotment and a well-behaved long-lived plugin would
        // trap with "all fuel consumed" mid-session. Refueling keys off
        // the pipe-progress counter rather than "any WASI call
        // completed": wasmtime's internal libcalls (`out_of_gas`,
        // `new_epoch`) also fire this hook (naively refilling on every
        // hook would refuel the guest at the exact moment it runs out of
        // gas — an infinite refuel loop), and a guest looping {{ pure
        // compute; clock_time_get() }} must not refuel either — cheap
        // metadata WASI calls are not protocol progress.
        // A well-behaved long-lived plugin must not refuel by spinning
        // cheap metadata WASI calls — and, symmetrically, must not be
        // killed by a fixed wall-clock lifetime either. Both budgets are
        // therefore RE-ARMED on protocol progress: fuel here, and the
        // epoch deadline below is re-set to a fresh `max_execution`
        // window on the same condition, turning the wall-clock cap into
        // a hang watchdog ("no protocol progress for max_execution")
        // rather than a lifetime limit. While the guest is parked inside
        // a blocking fd_read no guest code executes (no epoch checks),
        // and the ReturningFromHost hook re-arms the deadline before the
        // guest resumes, so idle-then-active plugins are never trapped.
        let max_fuel = limits.max_fuel;
        let rearm_ticks = limits.max_execution.map(|duration| {
            let ticks = duration.as_millis().max(1) / EPOCH_TICK.as_millis().max(1);
            u64::try_from(ticks).unwrap_or(u64::MAX).saturating_add(1)
        });
        let hook_progress = io_progress.clone();
        let mut seen_progress = 0u64;
        store.call_hook(move |mut store, hook| {
            if let CallHook::ReturningFromHost = hook {
                let progress = hook_progress.load(Ordering::Relaxed);
                if progress != seen_progress {
                    seen_progress = progress;
                    let _ = store.set_fuel(max_fuel);
                    if let Some(ticks) = rearm_ticks {
                        store.set_epoch_deadline(ticks);
                    }
                }
            }
            Ok(())
        });

        // Epoch deadlines are counted in ticks beyond the CURRENT epoch,
        // so this must be set immediately before the guest starts.
        let deadline = match limits.max_execution {
            Some(duration) => {
                let ticks = duration.as_millis().max(1) / EPOCH_TICK.as_millis().max(1);
                u64::try_from(ticks).unwrap_or(u64::MAX).saturating_add(1)
            }
            // Practically "never" — but a deadline must be set explicitly:
            // with epoch interruption enabled the default deadline is 0.
            None => u64::MAX / 2,
        };
        store.set_epoch_deadline(deadline);

        let mut linker = Linker::new(&self.engine);
        p1::add_to_linker_async(&mut linker, |state: &mut PluginState| &mut state.wasi)
            .map_err(|e| format!("failed to link WASI p1: {e}"))?;

        tokio::spawn(forward_stderr(host_stderr));

        // Only guests with a wall-clock deadline keep the epoch ticker
        // awake; the guard releases on every exit path (clean, trap, or
        // abort on shutdown), parking the ticker again.
        let guard = limits.max_execution.is_some().then(|| {
            self.epoch_demand.acquire();
            EpochDemandGuard(self.epoch_demand.clone())
        });
        let task = tokio::spawn(async move {
            let _demand = guard;
            run_module(store, linker, module).await
        });
        let peer = JsonRpcPeer::new(host_stdout, host_stdin, services);
        Ok(WasmPlugin {
            client: HostClient::new(peer.clone()),
            peer,
            task,
        })
    }
}

async fn run_module(
    mut store: Store<PluginState>,
    linker: Linker<PluginState>,
    module: Module,
) -> Result<(), String> {
    let instance = linker
        .instantiate_async(&mut store, &module)
        .await
        .map_err(|e| format!("failed to instantiate plugin module: {e:#}"))?;
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|e| format!("plugin module has no _start: {e}"))?;
    if let Err(error) = start.call_async(&mut store, ()).await {
        // WASI proc_exit(0) surfaces as an I32Exit "trap": a clean exit.
        if let Some(exit) = error.downcast_ref::<wasmtime_wasi::I32Exit>() {
            if exit.0 == 0 {
                return Ok(());
            }
            return Err(format!("plugin exited with code {}", exit.0));
        }
        return Err(format!("plugin trapped: {error:#}"));
    }
    Ok(())
}

/// Forward the guest's stderr to the host log, like v1 subprocess
/// plugins — including the same bounded line reader: a guest streaming
/// stderr without newlines must not grow the host buffer without bound
/// (BufReader::lines() has no limit).
async fn forward_stderr(reader: impl AsyncRead + Unpin) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        match tack_ext::process::read_line_bounded(
            &mut reader,
            &mut buf,
            tack_ext::process::OverCap::Discard,
        )
        .await
        {
            Ok(Some(line)) => tracing::info!(target: "tack_ext_wasm::plugin_stderr", "{line}"),
            Ok(None) => break,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                // Over-cap line: keep draining so the guest never blocks
                // on a full stderr pipe (same as v1).
                tracing::warn!(target: "tack_ext_wasm::plugin_stderr", "oversized stderr line dropped");
            }
            Err(e) => {
                // Genuine IO error: continuing would spin on a persistently
                // failing reader — the pipe is dead, so are we.
                tracing::warn!(target: "tack_ext_wasm::plugin_stderr", "stderr read failed: {e}");
                break;
            }
        }
    }
}

/// A live WASM plugin: its protocol peer plus the guest task. Dropping the
/// peer (or guest exit) closes the pipes and tears the other side down,
/// mirroring v1's process-death semantics.
pub struct WasmPlugin {
    /// Typed host → plugin calls (handshake, tools, hooks, …).
    pub client: HostClient,
    /// Protocol peer: same dead-peer/cancellation semantics as the
    /// process carrier.
    pub peer: Arc<JsonRpcPeer>,
    task: tokio::task::JoinHandle<Result<(), String>>,
}

impl std::fmt::Debug for WasmPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmPlugin")
            .field("alive", &self.peer.is_alive())
            .finish_non_exhaustive()
    }
}

impl WasmPlugin {
    /// Graceful shutdown: `shutdown` request, brief grace, then the guest
    /// task is aborted (wasmtime destruction is safe at any point — unlike
    /// a process force kill, there is no OS process to reap).
    pub async fn shutdown(mut self) {
        let _ = self.client.shutdown().await;
        if tokio::time::timeout(Duration::from_secs(2), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            // Await the abort: tokio drops the task future only once the
            // runtime processes the cancellation. The Store holds WASI
            // preopen directory handles, and on Windows an open handle
            // blocks deleting the extension directory (ERROR_SHARING_
            // VIOLATION) — returning before teardown races the caller's
            // cleanup.
            let _ = (&mut self.task).await;
        }
    }

    /// Await the guest's exit and report the outcome: `Err` on traps
    /// (fuel/epoch/memory limit violations), non-zero `proc_exit`, or
    /// instantiation failures.
    pub async fn wait(mut self) -> Result<(), String> {
        match (&mut self.task).await {
            Ok(result) => result,
            Err(join) => Err(format!("plugin task join failed: {join}")),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::Value;
    use tack_ext::rpc3::{
        HostCapabilities, HostInfo, InitializeParams, RunMode, ToolExecuteParams,
    };

    /// Records events; echoes requests back (plugin→host direction unused
    /// by the WAT fixtures below, but required by the trait).
    struct RecordingServices {
        events: tokio::sync::Mutex<Vec<(String, Value)>>,
    }

    impl RecordingServices {
        fn new() -> Arc<Self> {
            Arc::new(RecordingServices {
                events: tokio::sync::Mutex::new(Vec::new()),
            })
        }

        async fn wait_for_event(&self, name: &str) -> Value {
            for _ in 0..250 {
                {
                    let events = self.events.lock().await;
                    if let Some((_, payload)) = events.iter().find(|(event, _)| event == name) {
                        return payload.clone();
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("event {name} not received within 5s");
        }
    }

    #[async_trait::async_trait]
    impl PeerHandler for RecordingServices {
        async fn handle_request(
            &self,
            method: &str,
            params: Value,
        ) -> Result<Value, tack_ext::rpc3::ErrorObject> {
            Ok(serde_json::json!({ "method": method, "echo": params }))
        }
        async fn handle_notification(&self, method: &str, params: Value) {
            self.events.lock().await.push((method.to_string(), params));
        }
    }

    /// Minimal WASI p1 echo module: reads stdin chunks and writes them back
    /// to stdout verbatim until EOF. Proves the stdio plumbing end-to-end.
    const ECHO_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  ;; iovec at 0..8, nread at 12, nwritten at 8, data buffer at 16..4112.
  (func (export "_start")
    (loop $again
      (i32.store (i32.const 0) (i32.const 16))   ;; iov base
      (i32.store (i32.const 4) (i32.const 4096)) ;; iov len
      (drop (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 12)))
      (if (i32.eqz (i32.load (i32.const 12))) (then return)) ;; EOF
      (i32.store (i32.const 4) (i32.load (i32.const 12)))    ;; write back nread bytes
      (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))
      (br $again)))
)
"#;

    /// A scripted plugin: consumes one line (initialize), answers with
    /// `register_line`; consumes the next line (the first request, id 1),
    /// answers with `response_line`; then drains stdin until EOF.
    fn scripted_plugin_wat(register_line: &str, response_line: &str) -> String {
        fn wat_escape(s: &str) -> String {
            s.replace('\\', "\\\\").replace('"', "\\\"")
        }
        format!(
            r#"
(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  ;; Read one byte at a time (iovec 0..8 -> byte at 16, nread at 12) until
  ;; '\n'. Returns 1 if any byte was read, 0 on immediate EOF.
  (func $read_line (result i32)
    (local $got i32)
    (loop $loop
      (i32.store (i32.const 0) (i32.const 16))
      (i32.store (i32.const 4) (i32.const 1))
      (drop (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 12)))
      (if (i32.eqz (i32.load (i32.const 12))) (then (return (local.get $got))))
      (local.set $got (i32.const 1))
      (br_if $loop (i32.ne (i32.load8_u (i32.const 16)) (i32.const 10))))
    (local.get $got))
  (func $write (param $ptr i32) (param $len i32)
    (i32.store (i32.const 0) (local.get $ptr))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 12))))
  (data (i32.const 64) "{}\0a")
  (data (i32.const 1024) "{}\0a")
  (func (export "_start")
    (if (call $read_line) (then (call $write (i32.const 64) (i32.const {}))))
    (if (call $read_line) (then (call $write (i32.const 1024) (i32.const {}))))
    (loop $drain (br_if $drain (call $read_line))))
)
"#,
            wat_escape(register_line),
            wat_escape(response_line),
            register_line.len() + 1,
            response_line.len() + 1,
        )
    }

    const SPIN_WAT: &str = r#"(module (func (export "_start") (loop $l (br $l))))"#;

    /// A long-lived plugin that burns ~500k fuel per request: reads one
    /// line (any content), spins 100k iterations, answers with a canned
    /// `tick` event, and loops. Used to prove the fuel budget is
    /// replenished between requests rather than being a one-shot
    /// lifetime allotment.
    const SPIN_TICK_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  ;; Read one byte at a time (iovec 0..8 -> byte at 16, nread at 12) until
  ;; '\n'. Returns 1 if any byte was read, 0 on immediate EOF.
  (func $read_line (result i32)
    (local $got i32)
    (loop $loop
      (i32.store (i32.const 0) (i32.const 16))
      (i32.store (i32.const 4) (i32.const 1))
      (drop (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 12)))
      (if (i32.eqz (i32.load (i32.const 12))) (then (return (local.get $got))))
      (local.set $got (i32.const 1))
      (br_if $loop (i32.ne (i32.load8_u (i32.const 16)) (i32.const 10))))
    (local.get $got))
  ;; Burn fuel: ~5 instructions per iteration.
  (func $spin (param $n i32)
    (local $i i32)
    (block $done
      (loop $l
        (br_if $done (i32.ge_u (local.get $i) (local.get $n)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l))))
  (data (i32.const 64) "{\"jsonrpc\":\"2.0\",\"method\":\"tick\",\"params\":null}\0a")
  (func (export "_start")
    (loop $main
      (if (call $read_line)
        (then
          (call $spin (i32.const 100000))
          (i32.store (i32.const 0) (i32.const 64))   ;; iov base
          (i32.store (i32.const 4) (i32.const 48))   ;; iov len (47 chars + \n)
          (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 12)))
          (br $main)))))
)"#;

    /// 512 pages = 32 MiB minimum memory.
    const BIG_MEMORY_WAT: &str =
        r#"(module (memory (export "memory") 512) (func (export "_start")))"#;

    fn initialize_params() -> InitializeParams {
        InitializeParams {
            protocol_version: tack_ext::v3::PROTOCOL_VERSION.to_string(),
            host: HostInfo {
                name: "tack-ext-wasm-test".to_string(),
                version: "test".to_string(),
            },
            mode: RunMode::Tui,
            cwd: "/tmp".to_string(),
            trusted: true,
            capabilities: HostCapabilities::default(),
            config: None,
        }
    }

    /// Transport proof: a line written into the plugin's stdin comes back
    /// on its stdout, through a real wasmtime instance and WASI pipes, and
    /// is parsed by the shared PluginPeer read pump.
    #[tokio::test(flavor = "multi_thread")]
    async fn echo_module_roundtrips_a_line() {
        let carrier = WasmCarrier::new().unwrap();
        let services = RecordingServices::new();
        let plugin = carrier
            .spawn(
                ECHO_WAT.as_bytes(),
                &WasmLimits::default(),
                services.clone(),
            )
            .await
            .unwrap();

        plugin
            .peer
            .notify("initialize", serde_json::json!({"protocol": 3}))
            .await
            .unwrap();
        // The echo comes back as a notification with the SAME
        // method/payload and is dispatched to the handler by the pump.
        let payload = services.wait_for_event("initialize").await;
        assert_eq!(payload, serde_json::json!({"protocol": 3}));
        plugin.shutdown().await;
    }

    /// Protocol proof: the v3 handshake (initialize request/result) and a
    /// tools/execute request/response run over the WASM carrier.
    #[tokio::test(flavor = "multi_thread")]
    async fn handshake_and_tool_execute_over_wasm() {
        // The host sends initialize (id 1) then tools/execute (id 2).
        let register_line = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "protocolVersion": tack_ext::v3::PROTOCOL_VERSION,
                "plugin": {"name": "wat-script"},
                "capabilities": {
                    "tools": [{"name": "ping", "description": "Ping",
                               "parameters": {"type": "object", "properties": {}}}],
                },
            },
        })
        .to_string();
        let response_line = serde_json::json!({
            "jsonrpc": "2.0", "id": 2,
            "result": {"content": [{"type": "text", "text": "pong"}]},
        })
        .to_string();
        let wat = scripted_plugin_wat(&register_line, &response_line);

        let carrier = WasmCarrier::new().unwrap();
        let services = RecordingServices::new();
        let plugin = carrier
            .spawn(wat.as_bytes(), &WasmLimits::default(), services)
            .await
            .unwrap();

        let register = plugin
            .client
            .initialize(&initialize_params())
            .await
            .unwrap();
        assert_eq!(register.plugin.name, "wat-script");
        let tools = register.capabilities.tools.expect("tools declared");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "ping");

        let output = plugin
            .client
            .tool_execute(&ToolExecuteParams {
                name: "ping".to_string(),
                tool_call_id: "call_1".to_string(),
                arguments: serde_json::json!({}),
            })
            .await
            .unwrap();
        assert_eq!(output.content[0].text.as_deref(), Some("pong"));

        plugin.shutdown().await;
    }

    /// Fuel metering: a guest that never yields runs out of fuel and traps.
    #[tokio::test(flavor = "multi_thread")]
    async fn fuel_limit_traps_infinite_loop() {
        let carrier = WasmCarrier::new().unwrap();
        let services = RecordingServices::new();
        let limits = WasmLimits {
            max_fuel: 100_000,
            ..WasmLimits::default()
        };
        let plugin = carrier
            .spawn(SPIN_WAT.as_bytes(), &limits, services)
            .await
            .unwrap();
        let err = tokio::time::timeout(Duration::from_secs(10), plugin.wait())
            .await
            .expect("fuel-limited guest did not finish")
            .unwrap_err();
        assert!(err.contains("fuel"), "unexpected trap: {err}");
    }

    /// Fuel is replenished on every guest→host call, so `max_fuel`
    /// bounds PER-REQUEST work, not the plugin's lifetime: a plugin whose
    /// cumulative work across many requests far exceeds the budget (while
    /// each single request stays under it) must keep running instead of
    /// trapping with "all fuel consumed" mid-session.
    #[tokio::test(flavor = "multi_thread")]
    async fn fuel_is_replenished_between_requests() {
        const REQUESTS: usize = 10;
        let carrier = WasmCarrier::new().unwrap();
        let services = RecordingServices::new();
        let limits = WasmLimits {
            // One request burns ~500k fuel spinning, so ten requests burn
            // ~5M cumulatively — 5x the budget. Without replenishment the
            // guest dies a few requests in.
            max_fuel: 1_000_000,
            ..WasmLimits::default()
        };
        let plugin = carrier
            .spawn(SPIN_TICK_WAT.as_bytes(), &limits, services.clone())
            .await
            .unwrap();
        for _ in 0..REQUESTS {
            plugin.peer.notify("poke", Value::Null).await.unwrap();
        }
        let mut answered = 0;
        for _ in 0..500 {
            answered = services
                .events
                .lock()
                .await
                .iter()
                .filter(|(event, _)| event == "tick")
                .count();
            if answered == REQUESTS {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            answered, REQUESTS,
            "guest stopped mid-session (fuel treated as a lifetime budget?)"
        );
        plugin.shutdown().await;
    }

    /// A plugin manifest must not be able to weaken its own sandbox:
    /// declared limits above the host ceilings are clamped, modest limits
    /// pass through unchanged.
    #[test]
    fn manifest_limits_are_clamped_to_host_ceilings() {
        let greedy = WasmLimits {
            max_fuel: u64::MAX,
            max_memory_bytes: usize::MAX,
            max_execution: Some(Duration::from_secs(24 * 3600)),
        };
        let clamped = greedy.clamped();
        assert_eq!(clamped.max_fuel, HOST_MAX_FUEL);
        assert_eq!(clamped.max_memory_bytes, HOST_MAX_MEMORY_BYTES);
        assert_eq!(clamped.max_execution, Some(HOST_MAX_EXECUTION));

        // Modest limits pass through unchanged; `None` execution (an
        // explicit opt-out of the wall-clock cap) stays untouched.
        let modest = WasmLimits::default();
        assert_eq!(modest.clamped(), modest);
        assert_eq!(
            WasmLimits::default().max_execution,
            Some(DEFAULT_MAX_EXECUTION)
        );
        assert_eq!(
            WasmLimits {
                max_execution: None,
                ..WasmLimits::default()
            }
            .clamped()
            .max_execution,
            None
        );
    }

    /// A guest looping {{ pure compute; clock_time_get() }} must trap on
    /// fuel: only protocol-pipe byte movement replenishes the budget,
    /// cheap metadata WASI calls do not (regression: previously EVERY
    /// completed WASI call refueled, so this loop lived forever — and
    /// with the old `max_execution: None` default it had no wall-clock
    /// backstop either).
    const CLOCK_SPIN_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "clock_time_get" (func $clock (param i32 i64 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start")
    (loop $l
      (drop (call $clock (i32.const 0) (i64.const 0) (i32.const 0)))
      (br $l))))
"#;

    #[tokio::test(flavor = "multi_thread")]
    async fn metadata_wasi_calls_do_not_replenish_fuel() {
        let carrier = WasmCarrier::new().unwrap();
        let services = RecordingServices::new();
        let limits = WasmLimits {
            max_fuel: 1_000_000,
            ..WasmLimits::default()
        };
        let plugin = carrier
            .spawn(CLOCK_SPIN_WAT.as_bytes(), &limits, services)
            .await
            .unwrap();
        let err = tokio::time::timeout(Duration::from_secs(10), plugin.wait())
            .await
            .expect("metadata-call loop did not finish")
            .unwrap_err();
        assert!(err.contains("fuel"), "unexpected trap: {err}");
    }

    /// Epoch interruption: a wall-clock deadline traps the same guest.
    #[tokio::test(flavor = "multi_thread")]
    async fn epoch_deadline_traps_infinite_loop() {
        let carrier = WasmCarrier::new().unwrap();
        let services = RecordingServices::new();
        let limits = WasmLimits {
            max_execution: Some(Duration::from_millis(150)),
            ..WasmLimits::default()
        };
        let plugin = carrier
            .spawn(SPIN_WAT.as_bytes(), &limits, services)
            .await
            .unwrap();
        let err = tokio::time::timeout(Duration::from_secs(10), plugin.wait())
            .await
            .expect("epoch-limited guest did not finish")
            .unwrap_err();
        assert!(
            err.contains("epoch") || err.contains("interrupt"),
            "unexpected trap: {err}"
        );
    }

    /// Memory cap: instantiation fails when the module's minimum memory
    /// exceeds the store limit.
    #[tokio::test(flavor = "multi_thread")]
    async fn memory_limit_rejects_oversized_module() {
        let carrier = WasmCarrier::new().unwrap();
        let services = RecordingServices::new();
        let limits = WasmLimits {
            max_memory_bytes: 4 * 1024 * 1024,
            ..WasmLimits::default()
        };
        let plugin = carrier
            .spawn(BIG_MEMORY_WAT.as_bytes(), &limits, services)
            .await
            .unwrap();
        let err = plugin.wait().await.unwrap_err();
        assert!(
            err.to_lowercase().contains("memory"),
            "unexpected error: {err}"
        );
    }

    /// Module byte cap: oversized modules are rejected before compilation.
    #[tokio::test(flavor = "multi_thread")]
    async fn module_byte_cap_rejects_oversized_module() {
        let carrier = WasmCarrier::with_sandbox_caps(WasmSandboxCaps {
            max_module_bytes: 16, // far smaller than any real module
            ..WasmSandboxCaps::default()
        })
        .unwrap();
        let services = RecordingServices::new();
        let err = carrier
            .spawn(ECHO_WAT.as_bytes(), &WasmLimits::default(), services)
            .await
            .unwrap_err();
        assert!(err.contains("byte cap"), "unexpected error: {err}");
        // A module under the default cap passes this check (compilation
        // still happens; the echo module proves it end-to-end elsewhere).
        assert!(ECHO_WAT.len() <= WasmSandboxCaps::default().max_module_bytes);
    }

    /// Table caps: a module declaring a table larger than the element
    /// limit fails instantiation instead of allocating host-side.
    #[tokio::test(flavor = "multi_thread")]
    async fn table_limits_reject_oversized_table() {
        const BIG_TABLE_WAT: &str = r#"(module (table 1000 funcref) (func (export "_start")))"#;
        let carrier = WasmCarrier::with_sandbox_caps(WasmSandboxCaps {
            max_table_elements: 10,
            ..WasmSandboxCaps::default()
        })
        .unwrap();
        let services = RecordingServices::new();
        let plugin = carrier
            .spawn(BIG_TABLE_WAT.as_bytes(), &WasmLimits::default(), services)
            .await
            .unwrap();
        let err = plugin.wait().await.unwrap_err();
        assert!(
            err.to_lowercase().contains("table") || err.to_lowercase().contains("limit"),
            "unexpected error: {err}"
        );
    }

    /// Instance-count cap: a store that would exceed the instance limit is
    /// rejected by the limiter (here: the default config still allows the
    /// single plugin instance, so assert the knob is wired by shrinking it
    /// below one and observing instantiation fail).
    #[tokio::test(flavor = "multi_thread")]
    async fn instance_limit_below_one_rejects_instantiation() {
        let carrier = WasmCarrier::with_sandbox_caps(WasmSandboxCaps {
            max_instances: 0,
            ..WasmSandboxCaps::default()
        })
        .unwrap();
        let services = RecordingServices::new();
        let plugin = carrier
            .spawn(ECHO_WAT.as_bytes(), &WasmLimits::default(), services)
            .await
            .unwrap();
        let err = plugin.wait().await.unwrap_err();
        assert!(
            err.to_lowercase().contains("instance") || err.to_lowercase().contains("limit"),
            "unexpected error: {err}"
        );
    }

    // ------------------------------------------------------------------
    // Capability grants (WasmCapabilities / spawn_with_capabilities).
    //
    // Fixture convention: the guest asserts its own expectations and
    // reports the outcome via proc_exit — 0 = pass, a small nonzero code
    // identifies the failing step. `WasmPlugin::wait` maps exit 0 to Ok
    // and any other code to Err("plugin exited with code N"), so tests
    // just assert `wait().await.unwrap()`.
    // ------------------------------------------------------------------

    /// WASI rights bits requested at path_open time.
    const RIGHTS_FD_READ: u64 = 1 << 1;
    const RIGHTS_FD_WRITE: u64 = 1 << 6;

    /// Escape raw bytes as a WAT data-string body.
    fn wat_data(bytes: &[u8]) -> String {
        let mut out = String::new();
        for &b in bytes {
            match b {
                b'"' => out.push_str("\\\""),
                b'\\' => out.push_str("\\\\"),
                0x20..=0x7e => out.push(b as char),
                _ => out.push_str(&format!("\\{b:02x}")),
            }
        }
        out
    }

    /// Byte-compare helper shared by the fixtures below: 0 = equal.
    const MEMCMP_WAT: &str = r#"
  (func $memcmp (param $a i32) (param $b i32) (param $n i32) (result i32)
    (local $i i32)
    (block $done
      (loop $l
        (br_if $done (i32.ge_u (local.get $i) (local.get $n)))
        (if (i32.ne (i32.load8_u (i32.add (local.get $a) (local.get $i)))
                    (i32.load8_u (i32.add (local.get $b) (local.get $i))))
          (then (return (i32.const 1))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l)))
    (i32.const 0))
"#;

    /// path_open of `path` against preopen fd 3 must FAIL. Exit codes:
    /// 1 = open unexpectedly succeeded (sandbox leak).
    fn open_must_fail_wat(path: &str, rights: u64) -> String {
        format!(
            r#"
(module
  (import "wasi_snapshot_preview1" "path_open" (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "{path}")
  (func (export "_start")
    ;; path_open(fd=3, dirflags=0, path, len, oflags=0, rights, inheriting=0, fdflags=0, &fd=8)
    (if (i32.eqz (call $path_open (i32.const 3) (i32.const 0) (i32.const 16) (i32.const {plen})
                                  (i32.const 0) (i64.const {rights}) (i64.const 0) (i32.const 0) (i32.const 8)))
      (then (call $proc_exit (i32.const 1))))
    (call $proc_exit (i32.const 0))))
"#,
            path = wat_data(path.as_bytes()),
            plen = path.len(),
            rights = rights,
        )
    }

    /// path_open(fd 3) + fd_read of `path` must succeed and yield exactly
    /// `expected`. Exit codes: 2 open, 3 read, 4 length, 5 content.
    fn read_file_wat(path: &str, expected: &str) -> String {
        format!(
            r#"
(module
  (import "wasi_snapshot_preview1" "path_open" (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "{path}")
  (data (i32.const 4096) "{expected}")
{MEMCMP_WAT}
  (func (export "_start")
    (if (call $path_open (i32.const 3) (i32.const 0) (i32.const 16) (i32.const {plen})
                          (i32.const 0) (i64.const {rights}) (i64.const 0) (i32.const 0) (i32.const 8))
      (then (call $proc_exit (i32.const 2))))
    (i32.store (i32.const 64) (i32.const 1024))  ;; iov base
    (i32.store (i32.const 68) (i32.const 2048))  ;; iov len
    (if (call $fd_read (i32.load (i32.const 8)) (i32.const 64) (i32.const 1) (i32.const 72))
      (then (call $proc_exit (i32.const 3))))
    (if (i32.ne (i32.load (i32.const 72)) (i32.const {elen}))
      (then (call $proc_exit (i32.const 4))))
    (if (call $memcmp (i32.const 1024) (i32.const 4096) (i32.const {elen}))
      (then (call $proc_exit (i32.const 5))))
    (call $proc_exit (i32.const 0))))
"#,
            path = wat_data(path.as_bytes()),
            expected = wat_data(expected.as_bytes()),
            plen = path.len(),
            elen = expected.len(),
            rights = RIGHTS_FD_READ,
        )
    }

    /// A write to `path` under preopen fd 3 must fail at open OR write
    /// time. Exit codes: 1 = write unexpectedly succeeded (sandbox leak).
    fn write_must_fail_wat(path: &str) -> String {
        format!(
            r#"
(module
  (import "wasi_snapshot_preview1" "path_open" (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "{path}")
  (func (export "_start")
    ;; Open requesting FD_WRITE rights; if the open itself is denied, pass.
    (if (call $path_open (i32.const 3) (i32.const 0) (i32.const 16) (i32.const {plen})
                          (i32.const 0) (i64.const {rights}) (i64.const 0) (i32.const 0) (i32.const 8))
      (then (call $proc_exit (i32.const 0))))
    ;; The open succeeded — the write itself must still be denied.
    (i32.store (i32.const 64) (i32.const 1024))
    (i32.store (i32.const 68) (i32.const 4))
    (if (i32.eqz (call $fd_write (i32.load (i32.const 8)) (i32.const 64) (i32.const 1) (i32.const 72)))
      (then (call $proc_exit (i32.const 1))))
    (call $proc_exit (i32.const 0))))
"#,
            path = wat_data(path.as_bytes()),
            plen = path.len(),
            rights = RIGHTS_FD_WRITE,
        )
    }

    /// path_open(O_CREAT) + fd_write of `content` under preopen fd 3 must
    /// succeed. Exit codes: 2 open, 3 write, 4 short write, 5 close.
    fn create_file_wat(path: &str, content: &str) -> String {
        format!(
            r#"
(module
  (import "wasi_snapshot_preview1" "path_open" (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_close" (func $fd_close (param i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "{path}")
  (data (i32.const 1024) "{content}")
  (func (export "_start")
    ;; oflags=1 (O_CREAT), rights=FD_WRITE
    (if (call $path_open (i32.const 3) (i32.const 0) (i32.const 16) (i32.const {plen})
                          (i32.const 1) (i64.const {rights}) (i64.const 0) (i32.const 0) (i32.const 8))
      (then (call $proc_exit (i32.const 2))))
    (i32.store (i32.const 64) (i32.const 1024))
    (i32.store (i32.const 68) (i32.const {clen}))
    (if (call $fd_write (i32.load (i32.const 8)) (i32.const 64) (i32.const 1) (i32.const 72))
      (then (call $proc_exit (i32.const 3))))
    (if (i32.ne (i32.load (i32.const 72)) (i32.const {clen}))
      (then (call $proc_exit (i32.const 4))))
    (if (call $fd_close (i32.load (i32.const 8)))
      (then (call $proc_exit (i32.const 5))))
    (call $proc_exit (i32.const 0))))
"#,
            path = wat_data(path.as_bytes()),
            content = wat_data(content.as_bytes()),
            plen = path.len(),
            clen = content.len(),
            rights = RIGHTS_FD_WRITE,
        )
    }

    /// environ_sizes_get/environ_get must yield exactly `vars` (in grant
    /// order). Exit codes: 2 sizes_get, 3 count, 4 bufsize, 5 get, 6 data.
    fn env_probe_wat(vars: &[(String, String)]) -> String {
        let mut expected = Vec::new();
        for (key, value) in vars {
            expected.extend_from_slice(key.as_bytes());
            expected.push(b'=');
            expected.extend_from_slice(value.as_bytes());
            expected.push(0);
        }
        let data = if expected.is_empty() {
            String::new()
        } else {
            format!("  (data (i32.const 4096) \"{}\")\n", wat_data(&expected))
        };
        format!(
            r#"
(module
  (import "wasi_snapshot_preview1" "environ_sizes_get" (func $sizes (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_get" (func $get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
{data}{MEMCMP_WAT}
  (func (export "_start")
    (if (call $sizes (i32.const 0) (i32.const 4))
      (then (call $proc_exit (i32.const 2))))
    (if (i32.ne (i32.load (i32.const 0)) (i32.const {n}))
      (then (call $proc_exit (i32.const 3))))
    (if (i32.ne (i32.load (i32.const 4)) (i32.const {bsz}))
      (then (call $proc_exit (i32.const 4))))
    ;; envp array at 16, string buffer at 1024
    (if (call $get (i32.const 16) (i32.const 1024))
      (then (call $proc_exit (i32.const 5))))
    (if (call $memcmp (i32.const 1024) (i32.const 4096) (i32.const {bsz}))
      (then (call $proc_exit (i32.const 6))))
    (call $proc_exit (i32.const 0))))
"#,
            n = vars.len(),
            bsz = expected.len(),
        )
    }

    /// args_sizes_get/args_get must yield exactly `args`. Exit codes:
    /// 2 sizes_get, 3 argc, 4 bufsize, 5 get, 6 data.
    fn args_probe_wat(args: &[String]) -> String {
        let mut expected = Vec::new();
        for arg in args {
            expected.extend_from_slice(arg.as_bytes());
            expected.push(0);
        }
        let data = if expected.is_empty() {
            String::new()
        } else {
            format!("  (data (i32.const 4096) \"{}\")\n", wat_data(&expected))
        };
        format!(
            r#"
(module
  (import "wasi_snapshot_preview1" "args_sizes_get" (func $sizes (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "args_get" (func $get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 1)
{data}{MEMCMP_WAT}
  (func (export "_start")
    (if (call $sizes (i32.const 0) (i32.const 4))
      (then (call $proc_exit (i32.const 2))))
    (if (i32.ne (i32.load (i32.const 0)) (i32.const {n}))
      (then (call $proc_exit (i32.const 3))))
    (if (i32.ne (i32.load (i32.const 4)) (i32.const {bsz}))
      (then (call $proc_exit (i32.const 4))))
    ;; argv array at 16, string buffer at 1024
    (if (call $get (i32.const 16) (i32.const 1024))
      (then (call $proc_exit (i32.const 5))))
    (if (call $memcmp (i32.const 1024) (i32.const 4096) (i32.const {bsz}))
      (then (call $proc_exit (i32.const 6))))
    (call $proc_exit (i32.const 0))))
"#,
            n = args.len(),
            bsz = expected.len(),
        )
    }

    fn preopen(dir: &std::path::Path, guest_path: &str, access: PreopenAccess) -> PreopenGrant {
        PreopenGrant {
            host_path: dir.to_path_buf(),
            guest_path: guest_path.to_string(),
            access,
        }
    }

    async fn spawn_and_wait(wat: &str, capabilities: &WasmCapabilities) -> Result<(), String> {
        let carrier = WasmCarrier::new().unwrap();
        let plugin = carrier
            .spawn_with_capabilities(
                wat.as_bytes(),
                &WasmLimits::default(),
                RecordingServices::new(),
                capabilities,
            )
            .await?;
        tokio::time::timeout(Duration::from_secs(10), plugin.wait())
            .await
            .map_err(|_| "guest did not finish within 10s".to_string())?
    }

    /// Regression: with no grants (the `spawn` default), the guest has no
    /// preopen fd at all and path_open fails.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_grants_guest_cannot_open_files() {
        let result = spawn_and_wait(
            &open_must_fail_wat("hello.txt", RIGHTS_FD_READ),
            &WasmCapabilities::default(),
        )
        .await;
        result.unwrap();
    }

    /// ReadOnly preopen: the guest reads a host file through fd 3 and
    /// verifies its content byte-for-byte inside the sandbox.
    #[tokio::test(flavor = "multi_thread")]
    async fn readonly_preopen_allows_reading() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hello from the host").unwrap();
        let capabilities = WasmCapabilities {
            preopens: vec![preopen(dir.path(), "/data", PreopenAccess::ReadOnly)],
            ..WasmCapabilities::default()
        };
        let result = spawn_and_wait(
            &read_file_wat("hello.txt", "hello from the host"),
            &capabilities,
        )
        .await;
        result.unwrap();
    }

    /// ReadOnly preopen: writes are rejected (at open or write time).
    #[tokio::test(flavor = "multi_thread")]
    async fn readonly_preopen_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hello from the host").unwrap();
        let capabilities = WasmCapabilities {
            preopens: vec![preopen(dir.path(), "/data", PreopenAccess::ReadOnly)],
            ..WasmCapabilities::default()
        };
        let result = spawn_and_wait(&write_must_fail_wat("hello.txt"), &capabilities).await;
        result.unwrap();
        // The host file is untouched.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
            "hello from the host"
        );
    }

    /// ReadWrite preopen: the guest creates a new file in the granted
    /// directory; the host observes it after the guest exits.
    #[tokio::test(flavor = "multi_thread")]
    async fn readwrite_preopen_allows_creating_files() {
        let dir = tempfile::tempdir().unwrap();
        let capabilities = WasmCapabilities {
            preopens: vec![preopen(dir.path(), "/data", PreopenAccess::ReadWrite)],
            ..WasmCapabilities::default()
        };
        let result = spawn_and_wait(
            &create_file_wat("created.txt", "created by the guest"),
            &capabilities,
        )
        .await;
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("created.txt")).unwrap(),
            "created by the guest"
        );
    }

    /// Capability scoping: paths escaping the granted directory (`..`)
    /// are rejected even though the parent is readable by this process.
    #[tokio::test(flavor = "multi_thread")]
    async fn preopen_escape_beyond_granted_dir_is_denied() {
        let parent = tempfile::tempdir().unwrap();
        std::fs::write(parent.path().join("outside.txt"), "secret").unwrap();
        let inner = parent.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        let capabilities = WasmCapabilities {
            preopens: vec![preopen(&inner, "/data", PreopenAccess::ReadOnly)],
            ..WasmCapabilities::default()
        };
        let result = spawn_and_wait(
            &open_must_fail_wat("../outside.txt", RIGHTS_FD_READ),
            &capabilities,
        )
        .await;
        result.unwrap();
    }

    /// A preopen grant for a host path that does not exist fails the
    /// spawn before instantiation.
    #[tokio::test(flavor = "multi_thread")]
    async fn preopen_of_missing_host_path_fails_spawn() {
        let carrier = WasmCarrier::new().unwrap();
        let capabilities = WasmCapabilities {
            preopens: vec![PreopenGrant {
                host_path: std::path::PathBuf::from("/definitely/not/a/real/dir"),
                guest_path: "/data".to_string(),
                access: PreopenAccess::ReadOnly,
            }],
            ..WasmCapabilities::default()
        };
        let err = carrier
            .spawn_with_capabilities(
                ECHO_WAT.as_bytes(),
                &WasmLimits::default(),
                RecordingServices::new(),
                &capabilities,
            )
            .await
            .unwrap_err();
        assert!(err.contains("failed to preopen"), "unexpected error: {err}");
    }

    /// Env grants are visible via environ_get, in grant order; the default
    /// sandbox still exposes an empty environment.
    #[tokio::test(flavor = "multi_thread")]
    async fn env_grant_is_visible_via_environ_get() {
        let granted = WasmCapabilities {
            env: vec![
                ("TACK_WASM_TEST".to_string(), "capabilities-ok".to_string()),
                ("SECOND".to_string(), "2".to_string()),
            ],
            ..WasmCapabilities::default()
        };
        let result = spawn_and_wait(
            &env_probe_wat(&[
                ("TACK_WASM_TEST".to_string(), "capabilities-ok".to_string()),
                ("SECOND".to_string(), "2".to_string()),
            ]),
            &granted,
        )
        .await;
        result.unwrap();

        // Default sandbox: no environment at all.
        let result = spawn_and_wait(&env_probe_wat(&[]), &WasmCapabilities::default()).await;
        result.unwrap();
    }

    /// Arg grants are visible via args_get, in grant order.
    #[tokio::test(flavor = "multi_thread")]
    async fn args_grant_is_visible_via_args_get() {
        let capabilities = WasmCapabilities {
            args: vec!["plugin".to_string(), "--verbose".to_string()],
            ..WasmCapabilities::default()
        };
        let result = spawn_and_wait(
            &args_probe_wat(&["plugin".to_string(), "--verbose".to_string()]),
            &capabilities,
        )
        .await;
        result.unwrap();

        // Default sandbox: empty argv.
        let result = spawn_and_wait(&args_probe_wat(&[]), &WasmCapabilities::default()).await;
        result.unwrap();
    }

    /// Network grants are accepted by the ctx builder without panicking
    /// and are inert for p1 guests (no socket imports exist in the p1
    /// ABI): the module still runs normally with all of them enabled.
    #[tokio::test(flavor = "multi_thread")]
    async fn network_grants_are_accepted_but_inert_for_p1() {
        let carrier = WasmCarrier::new().unwrap();
        let services = RecordingServices::new();
        let capabilities = WasmCapabilities {
            network: NetworkGrants {
                allow_tcp: true,
                allow_udp: true,
                allow_dns: true,
            },
            ..WasmCapabilities::default()
        };
        let plugin = carrier
            .spawn_with_capabilities(
                ECHO_WAT.as_bytes(),
                &WasmLimits::default(),
                services.clone(),
                &capabilities,
            )
            .await
            .unwrap();
        plugin
            .peer
            .notify("initialize", serde_json::json!({"protocol": 3}))
            .await
            .unwrap();
        let payload = services.wait_for_event("initialize").await;
        assert_eq!(payload, serde_json::json!({"protocol": 3}));
        plugin.shutdown().await;
    }
}
