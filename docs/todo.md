# To-Do List

**English | [简体中文](todo.zh-CN.md)**

Tack's manual / cross-repo to-do items (TODOs in code live in source
comments; this file only tracks items that require action outside the repo
or coordination across steps).

## To Do

### 1. Submit Tack to the ACP Registry (manual PR)

**Background**: the ACP protocol itself has no icon field; agent icons and
one-click install in clients like Zed/JetBrains come from the
[ACP Registry](https://agentclientprotocol.com/get-started/registry).
Submission materials are ready: `assets/acp-registry/tack/` (compliant
monochrome icon `icon.svg` + `agent.json` drafts for 6 platforms + detailed
instructions in `assets/acp-registry/README.md`).

**Steps**:

1. Fork <https://github.com/agentclientprotocol/registry>
2. Copy the entire `assets/acp-registry/tack/` directory to the fork's repo
   root (the directory name `tack` must match the `id` in `agent.json`)
3. Replace all `FILL_FROM_SHA256SUMS` in `agent.json` with the actual
   hashes from the target release's `SHA256SUMS.txt`; align `version` and
   the tag version in the download URLs
4. Open a PR; wait for CI validation (schema + icon rules) and manual
   review

**Follow-up (every release)**: the registry entry points at a fixed
version; after each new release, open another PR updating `version` / URL /
sha256. After a few stable cycles, consider folding "update ACP registry
entry" into the `docs/release.md` release checklist (or semi-automate it
with a script).

**Requires**: repo-owner privileges (fork + PR submitted on behalf of the
project).

## Done

| Date | Item |
|---|---|
| 2026-09-25 | ACP `initialize` now reports `agentInfo` (name/title/version), effective with the next release (217bd14) |
