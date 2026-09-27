# Ubuntu check Wedge Postmortem (the "phantom wedge" incident)

**English | [简体中文](ci-ubuntu-wedge.zh-CN.md)**

In late 2026-09, the CI ubuntu `check` job wedged **completely silently** in the
Test step multiple times in a row: no error, no timeout, no logs — until it was
reaped by the job timeout or a manual cancel. The macOS/Windows test-cross jobs
ran the exact same tests **green every time**, and it never reproduced locally.
This doc records the symptoms, the investigation path, the **four chained root
causes** finally proven, the fixes, and the reusable forensics methods.

Final fixes: `aec16b2` (git subprocesses), `9637f88` (kill_process_tree),
`9960e88`/`7490d99` (cache and build resources), `6fd492f` onward (Docker
containment), `19d5995` (removing the temporary scaffolding).

## Symptoms

- The `check` job (ubuntu) Test step printed a few test lines, then **stopped
  dead**: zero output for tens of minutes, the step stuck in in_progress
  forever, and after ~50 minutes GitHub reaped it with
  `lost communication with the server`.
- **The wedge point drifted run to run**: vertex_adc_tests 3/5, somewhere in
  tack-app extension_host, even mid full-compile — never the same place twice,
  but always on ubuntu.
- **Logs were obliterated**: the postmortem log zip never contained the check
  job; pulling the job-log API directly returned `BlobNotFound`. The live view
  showed the last few lines that streamed out, and nothing more.

## Root causes (four, covering for each other)

The chained multi-bug nature is why this investigation was expensive: each bug
masked or faked the crime scene of the next.

### 1. git tests: bare `.status()` + inherited stdio → orphaned pipe holders (`aec16b2`)

The `extension_host` git tests (clone/checkout/init/commit) spawned git with a
bare `std::process::Command::status()`: **no timeout, and stdout/stderr
inherited from the test process**. Once a git child refused to exit:

1. the test thread blocked on `.status()` forever;
2. after the test framework/nextest/timeout killed the test process or
   supervisor, **the orphaned git child still held the inherited stdout pipe**;
3. the pipe never reached EOF → drain loops/runner/`tee` all waited forever →
   total silence.

This mechanism explains both "why GNU timeout didn't help" (it only kills the
direct child) and "why a manual cancel lost all logs". Fix: route everything
through `sync_process::output_with_timeout` (pipe isolation + null stdin +
hard timeout + kill on timeout).

**Lesson: whenever you spawn a subprocess, either isolate its pipes or bound
its lifetime; never `.status()`-wait on a subprocess with no timeout and
inherited stdio in a process that can be killed.**

### 2. kill_process_tree depended on an external `kill` binary (`9637f88`)

`tack-tools::shell::kill_process_tree` was implemented by shelling out to the
`kill` **binary** (from the procps package). Minimal environments (slim docker
images, Termux) don't have procps, so group-kill and single-kill **both failed
silently** — after an exec timeout the child kept running to completion
(`ext_headless::exec_timeout_kills_the_child` therefore hung for 60 seconds in
the container, and **never appeared in any failed run's logs** — which is
exactly what exposed it).

This wasn't just a CI problem: for every user running Tack in a container or
minimal system, exec-timeout semantics were broken. Fix: use the `kill`/`killpg`
syscalls via `nix`, with no external binary dependency.

**Lesson: when investigating exec-style timeouts, first check whether the target
environment has procps; to prove "some test never finished", `grep -c <test-name>`
against the full log.**

### 3. Build-cache death spiral (`9960e88`)

`Swatinem/rust-cache` **only saves the cache on job success by default**
(`CACHE_ON_FAILURE: false`). check failing red / being cancelled repeatedly →
cache stuck in the distant past → every run downloaded and compiled a
wasmtime-scale dependency tree from scratch (4 vCPU/16GB debug) → slower, more
likely to time out / get cancelled → the cache could never warm up.

**The statistical explanation for the "drifting wedge point"**: cache warmth
differed per run — on a warm cache the job made it into the test phase (dying
to bug 1/2); on a cold cache it died of compile-time resource exhaustion. Fix:
`cache-on-failure: true` (red runs warm the cache too), plus enough headroom
for cold builds (job timeout 45→60 minutes).

### 4. Giant debug builds vs. a tiny runner (`7490d99`)

This repo's dev test binaries are about **765–816MB each** (debug=2 DWARF);
20+ test binaries plus intermediate artifacts need several times the ~14GB of
usable SSD on a GH ubuntu runner for a full build. Disk fills up → writes
block → **the VM freezes and the runner goes dark** — this is the direct cause
of the "lost communication" and the total loss of logs/artifacts.

Fix: a Free disk space step (delete the image's bundled
dotnet/android/ghc/powershell/swift, reclaiming 20–30GB); and in the Test step
`CARGO_PROFILE_DEV_DEBUG=0` (no DWARF, binaries drop to the ~100MB range;
panic/assert messages carry static file:line anyway, so debugging is
unaffected) + `CARGO_BUILD_JOBS=3` (cap peak parallel-link memory).

## Final CI shape (and why it looks this way)

See the check job in `.github/workflows/ci.yml` and `.config/nextest.toml`:

- **Docker containment**: the test workload runs inside an `ubuntu:24.04`
  container with hard cgroup limits `--memory=12g --cpus=3.5 --pids-limit=2048`.
  A resource bomb only blows up the container; the runner agent stays alive to
  stream the failure out — **every failure now comes with logs**. Inside the
  container, apt installs `git build-essential pkg-config` (the image has no
  git/C toolchain; git must be installed with `--no-install-recommends`,
  otherwise Recommends pulls in ca-certificates, whose postinst writes to the
  read-only-mounted `/etc/ssl/certs` and fails).
- **cargo-nextest**: one process per test + `profile.ci` slow-timeout (SLOW at
  60s, killed after 180s+grace) — a wedged test gets **named** instead of
  hanging silently. Doctests are outside nextest's coverage and run separately
  via `cargo test --doc`.
- **Dual-channel logging**: `tee test-output.log` +
  `actions/upload-artifact` (`if: always()`). Even if the step fails, the full
  log is preserved.
- **Don't cancel jobs manually**: cancelling discards the job's entire log
  (BlobNotFound); the `if: always()` artifact step only runs after the "gentle"
  paths (failure/manual cancel), not after GitHub's timeout hard-kill.

## Investigation methods retrospective (reusable)

What worked:

1. **Guarantee "every failure leaves a log" first, then talk about
   localization.** On the bare runner every run was a write-off; after
   containerization each run produced one visible failure — solved in three
   runs.
2. **The live stream is the only survivor**: after the VM dies, only the lines
   that streamed into GitHub's live view while it was alive remain (the API
   can't fetch them, but a user can see/paste them).
3. **The `if: always()` artifact step**: still runs after gentle
   failures/manual cancels — a stable forensics channel.
4. **The "some test never finished" test**: `grep -c <test-name>` against the
   full log; 0 occurrences means prime suspect. That's how exec_timeout was
   caught.
5. **Distinguish "test wedged" from "runner dead"**: the former leaves a body
   via timeout machinery (SLOW/FAIL lines); the latter can't even schedule the
   watchdog's `sleep` — if the watchdog doesn't fire when due, the VM is dead.
6. **Bisect by exclusion with the right match dimension**: nextest's
   `test(wasm)` only matches test names; excluding a whole crate's wasmtime
   tests needs `not (test(wasm) or package(tack-ext-wasm))`.

What didn't work (don't retry these):

- Adding any in-job "timeout/watchdog" against runner death — it dies on the
  same VM as the culprit.
- Local stress-loop reproduction — architecture (aarch64/x86_64), core count,
  proot branches, and load all differ; concurrency/resource bugs are
  probabilistic, and "30 local runs all passed" proves nothing.
- Guessing mechanisms and patching them — the first four fix attempts
  (current-thread starvation, RSA keygen, reqwest timeouts, 100-continue) were
  all unverified guesses, and all of them were "fixed but not cured".
