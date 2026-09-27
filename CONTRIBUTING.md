# Contributing to Tack

Thanks for your interest in contributing! Tack is a Rust reimplementation of
the [pi](https://github.com/earendil-works/pi) coding agent. This document
covers everything you need to get a change from idea to merged pull request.

## Development setup

- Install Rust via [rustup](https://rustup.rs/). The repository contains a
  `rust-toolchain.toml` that pins the exact toolchain (including `rustfmt`
  and `clippy`), so `rustup` selects it automatically — no manual toolchain
  management needed.
- The project's minimum supported Rust version (MSRV) is **1.85**; the crate
  uses edition 2024.
- Optional but recommended: [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny)
  (`cargo install cargo-deny`). CI runs it against advisories, bans, licenses,
  and sources using the repository's `deny.toml`. Running `cargo deny check`
  locally before opening a dependency-changing PR saves a CI round-trip.

## Common commands

Use the exact commands CI runs so local results match:

```sh
# Build the release binary (the tack CLI lives in the tack-app crate)
cargo build --release -p tack-app

# Run the full test suite
cargo test --locked --workspace

# Formatting (check-only, as CI does)
cargo fmt --all -- --check

# Lints — warnings are errors
cargo clippy --locked --workspace --all-targets -- -D warnings

# Documentation — rustdoc warnings are errors
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

# Dependency policy (optional locally, enforced in CI)
cargo deny check
```

## Pull request process

1. **Branch.** Create a topic branch from `main`
   (e.g. `fix/context-overflow` or `feat/memory-recall`).
2. **Commits.** Use [Conventional Commits](https://www.conventionalcommits.org/).
   Examples from the project history: `feat(memory): ...`,
   `fix(context): ...`, `docs(directories): ...`. Common types: `feat`,
   `fix`, `docs`, `refactor`, `test`, `chore`, `perf`.
3. **CHANGELOG entry.** `CHANGELOG.md` is a hard convention: the format uses
   `## [x.y.z]` headers, and the TUI's `/changelog` command and the startup
   "what's new" notice parse it. Add every user-notable change as a bullet
   under the `## [Unreleased]` section.
4. **CI must be fully green.** CI runs on Linux, Windows, and macOS and
   includes `cargo fmt --all -- --check`,
   `cargo clippy --locked --workspace --all-targets -- -D warnings`,
   `cargo test --locked --workspace` on all three platforms,
   `cargo doc --no-deps` with `RUSTDOCFLAGS="-D warnings"`, and
   `cargo deny check`. A red job on any platform blocks merge.
5. Keep PRs focused. One logical change per PR makes review and potential
   reverts much easier.

## Testing conventions

- Unit and integration tests **must not depend on the network**. Mock or
  fixture anything that would otherwise hit a live provider or remote
  service.
- The `evals/` directory contains agent-behavior evaluation tasks (each is a
  directory with a `task.json`, fixtures, and a verifier). If you add or
  modify an eval task, you **must** run `evals/selftest.sh` afterwards. It is
  pure shell + python3 and verifies that each task's verifier fails on the
  unsolved fixture and passes after applying the reference solution.
- `evals/docs-audit/static_check.sh` cross-checks docs against code
  (settings keys, hook events, CLI flags, feature gates). CI fails on
  documented-but-not-implemented drift; when docs legitimately mention
  external tools' flags/keys, add them to the script's `EXCLUDE_*` lists
  with a reason.
- The ubuntu `check` job runs the suite inside Docker with cgroup caps
  and uploads `test-output.log` as an artifact. If it ever hangs or
  dies log-less again, do NOT cancel the job — cancellation discards its
  logs. The full incident write-up lives in
  [docs/ci-ubuntu-wedge.md](docs/ci-ubuntu-wedge.md).

## Benchmarks and fuzzing

Microbenchmarks live next to the crates they measure
(`crates/tack-session/benches/session_parse.rs`,
`crates/tack-protocol/benches/cbor_framing.rs`,
`crates/tack-tui/benches/render.rs`), built on criterion. Run them with:

```bash
cargo bench --workspace --bench session_parse --bench cbor_framing --bench render
```

CI compiles benches on every PR (clippy `--all-targets`) and runs them
weekly (`.github/workflows/bench.yml`), comparing against the previous
week's baseline. If you touch a hot path (session parsing, CBOR framing,
the diff renderer), run the matching bench before/after and paste the
criterion change summary into your PR.

Fuzz targets live in `fuzz/` (session JSONL lines, CBOR framing, SSE
streams, extension NDJSON). Run them locally with
`cargo +nightly fuzz run <target>`; CI smoke-runs every target weekly.

## Platform-specific code

The codebase contains platform-specific paths, notably:

- **Windows**: `windows-sys`-based sandboxing; bash tool calls go through
  Git Bash.
- **macOS**: seatbelt sandbox profiles.
- **Linux**: bubblewrap (`bwrap`) sandboxing.

If your change touches `cfg`-gated or otherwise platform-conditional code,
verify it on the affected platform(s) if you can. CI runs clippy and tests on
all three platforms and will catch cross-platform breakage, but behavioral
verification (especially around sandboxing) on the real platform is strongly
encouraged. Note platform coverage in your PR description.

## Documentation language policy

- **Contributor-facing entry points are written in English**: `README.md`,
  `CONTRIBUTING.md`, `SECURITY.md`, issue/PR templates, rustdoc, and code
  comments. `README.zh-CN.md` is a Chinese translation of `README.md` for
  readers; when you change `README.md`, update `README.zh-CN.md` in the same
  PR to keep the two in sync.
- **In-depth design documents under `docs/` are bilingual**: `foo.md` is the
  English canonical and `foo.zh-CN.md` the Chinese version. Write new docs in
  English, add the zh-CN translation in the same PR, and keep both versions in
  sync on later edits (Chinese files link to `.zh-CN.md` cross-doc targets).

## Architecture orientation

New to the codebase? Start here:

- [`docs/onboarding.md`](docs/onboarding.md) — guided tour of the workspace.
- [`docs/architecture.md`](docs/architecture.md) — how the nine crates
  (`tack-ai`, `tack-agent-core`, `tack-session`, `tack-tools`, `tack-tui`,
  `tack-protocol`, `tack-ext`, `tack-ext-wasm`, `tack-app`) fit together.

## License

Tack is licensed under the [Apache License 2.0](LICENSE). By contributing, you
agree that your contributions will be licensed under the same terms.

Tack is a Rust reimplementation of the [pi](https://github.com/earendil-works/pi)
coding agent; pi itself is MIT-licensed © 2025 Mario Zechner.
