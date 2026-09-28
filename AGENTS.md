# AGENTS.md

Guidance for AI coding agents (and humans) working in this repository.

## Project overview

Tack is a Rust reimplementation of the TypeScript [pi](https://github.com/earendil-works/pi)
coding agent — interactive TUI, headless print mode, ACP editor integration,
JSONL RPC, and remote sessions. **Wire- and storage-compatibility with TS pi is
a hard constraint**: session files (v1–v4), RPC/ACP wire protocols, provider
registry, and CLI flags must stay compatible. Never change a persisted format
or wire protocol without a migration path and a note in `docs/compatibility.md`.

## Workspace layout

Nine crates, acyclic dependency direction (`tack-app` depends on all),
plus an `xtask` automation crate (no library role):

| Crate | Role |
|---|---|
| `tack-ai` | Model layer: unified message types, API adapters (Anthropic, OpenAI, Google, …), OAuth, provider registry + embedded 1130-model catalog |
| `tack-agent-core` | Agent loop, `AgentEvent`, hooks, extension/tool traits |
| `tack-session` | JSONL session persistence (tree structure), compaction |
| `tack-tools` | Built-in tools: read/bash/edit/write/grep/find/ls, LSP, MCP |
| `tack-tui` | Terminal UI library (components, markdown, syntax highlighting) |
| `tack-protocol` | CBOR schemas/framing for remote sessions, `RemoteClient` |
| `tack-ext` | Subprocess plugin protocol (NDJSON over stdio) |
| `tack-ext-wasm` | Sandboxed WASM extensions |
| `tack-app` | The `tack` binary: CLI, TUI app, print/ACP/RPC/serve modes |
| `xtask` | Repo automation: OpenRPC → Rust codegen for tack-RPC v3 (`cargo run -p xtask -- codegen`, `--check` in CI) |

Start with `docs/onboarding.md` and `docs/architecture.md` when lost.

## Commands (match CI exactly)

```sh
cargo build --release -p tack-app              # the tack binary
cargo test --locked --workspace                # full test suite
cargo fmt --all -- --check                     # formatting (check-only)
cargo clippy --locked --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo deny check                               # dependency policy (CI-enforced)
```

- Toolchain is pinned by `rust-toolchain.toml` (MSRV 1.85, edition 2024);
  rustup picks it automatically. Never bump it casually.
- `Cargo.lock` is committed and CI builds with `--locked`. After changing
  dependencies or `workspace.package.version`, regenerate the lockfile
  (e.g. `cargo check`) and commit it.

## Conventions

- **Commits / PRs**: [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat(scope):`, `fix(scope):`, `docs:`, `refactor:`, `test:`, `chore:`, `perf:`).
  One logical change per PR; CI green on Linux + Windows + macOS.
- **CHANGELOG**: hard convention. `CHANGELOG.md` uses `## [x.y.z]` headers and
  is parsed by the TUI `/changelog` command. Record every user-notable change.
- **Comments / rustdoc**: explain *why*, not *what*. Doc-comments are checked
  with `RUSTDOCFLAGS="-D warnings"`.

## Testing rules

- Tests **must not touch the network** — mock or fixture providers/services.
- Touching anything under `evals/` → run `evals/selftest.sh` afterwards
  (verifies each task's verifier fails unsolved and passes solved).
- Changing settings keys, hook events, CLI flags, or feature gates → keep docs
  in sync; `evals/docs-audit/static_check.sh` (runs in CI) fails on
  documented-but-not-implemented drift.
- Hot paths (session parsing, CBOR framing, diff renderer) have criterion
  benches; run the matching bench before/after perf-sensitive changes.

## Platform notes

- Windows: `windows-sys` sandboxing; bash tool calls go through Git Bash.
- macOS: seatbelt sandbox profiles. Linux: bubblewrap (`bwrap`).
- `cfg`-gated changes should be verified on the affected platform; CI runs
  clippy + tests on all three.

## Documentation language policy

- English for contributor-facing entry points: `README.md`, this file,
  `CONTRIBUTING.md`, `SECURITY.md`, rustdoc, code comments.
- `README.zh-CN.md` mirrors `README.md` — update both in the same change.
- Every doc under `docs/` is bilingual: `foo.md` is the English canonical,
  `foo.zh-CN.md` the Chinese version (same convention as the READMEs). Write
  new docs in English, add the zh-CN translation in the same change, and keep
  both in sync. In Chinese files, cross-doc links point to the `.zh-CN.md`
  targets; in English files, to the canonical `.md` targets.

## Releasing

Maintainers only — see `docs/release.md`. In short: bump
`workspace.package.version` in `Cargo.toml` (single source of truth),
regenerate `Cargo.lock`, add the `## [x.y.z]` CHANGELOG entry, commit as
`release: vX.Y.Z`, then tag `tack-vX.Y.Z` (must match the version exactly) and
push the tag; GitHub Actions builds 6 platforms and publishes the release.
