<!--
Thanks for contributing! Keep the description focused: what changed and why.
See CONTRIBUTING.md for the full process.
-->

## Description

<!-- What does this PR change, and why? -->

## Related issues

<!-- e.g. "Fixes #123" or "Related to #456" -->

## Checklist

- [ ] `cargo fmt --all -- --check` passes locally
- [ ] `cargo clippy --locked --workspace --all-targets -- -D warnings` passes locally
- [ ] `cargo test --locked --workspace` passes locally
- [ ] Added a `CHANGELOG.md` entry under `## [Unreleased]` for user-notable changes
- [ ] Updated documentation (README / docs/ / rustdoc) where behavior or configuration changed
- [ ] Cross-platform impact self-assessed (Windows / macOS / Linux): noted below if the change touches platform-specific code (sandboxing, paths, Git Bash); CI runs clippy + tests on all three platforms
- [ ] If an eval task in `evals/` was added or modified: ran `evals/selftest.sh` successfully

## Platform-specific notes (if any)

<!-- Which platforms were the changes verified on? Anything CI cannot catch? -->
