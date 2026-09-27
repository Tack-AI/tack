# Telemetry & data collection: design decision

**English | [简体中文](telemetry.zh-CN.md)**

**Status: decided — Tack ships with no telemetry of any kind.**

## Decision

Tack does not collect, transmit, or phone home any usage data, crash
reports, metrics, or analytics. There is no telemetry endpoint in the
codebase and no build-time or runtime flag that enables one.

## Rationale

- **Privacy by default.** A coding agent sits in an unusually sensitive
  position: it reads source code (often proprietary or confidential), shell
  histories, file paths, credentials-adjacent config, and the full text of
  user prompts. Even "anonymous" telemetry from such a tool risks leaking
  information users never intended to share — a stack trace can embed a
  file path, a feature-usage event can reveal that a secret project exists.
- **Trust.** Users should be able to run the agent on sensitive codebases
  without auditing network traffic to feel safe. "No telemetry" is a
  property that is easy to state, easy to verify, and impossible to
  misunderstand — unlike "telemetry is anonymized" or "you can opt out".
- **Simplicity.** No consent flows, no data-retention questions, no
  compliance surface area.

## What we have instead

Troubleshooting relies on strictly local, user-controlled mechanisms:

- **`tack doctor`** — an environment self-check (auth state, provider
  configuration, sandbox support, paths) that users run themselves and can
  paste into a bug report. This is why the bug-report issue template asks
  for its full output.
- **`~/.tack/agent/crash.log`** — panics are recorded to this local file
  automatically. It never leaves the machine unless the user chooses to
  share excerpts.
- **Opt-in local tracing** — setting `observability.enabled` in settings
  (or `TACK_TRACE_FILE=1`) writes a local JSONL trace of agent activity
  for debugging. It is off by default, writes only to the local disk, and
  is fully under the user's control.

## The cost (acknowledged honestly)

Flying blind is a real trade-off:

- We **cannot measure real-world crash rates** — we only learn about
  crashes from users who take the time to file an issue with their
  `crash.log`.
- We **have no feature-usage data** — decisions about where to invest
  (which providers, which tools, which TUI features) are driven by issue
  volume, discussions, and maintainer judgment rather than evidence about
  what users actually do.

We accept these costs as the price of the privacy guarantee.

## Conditions for any future opt-in crash reporting

If the project ever considers adding crash reporting, all of the following
must hold — anything less is a regression of this decision:

1. **Explicit opt-in only.** Off by default; enabling requires an
   affirmative user action (a settings key), never a "skip to keep enabled"
   default.
2. **Minimal payload.** Only the panic location (crate, function, line),
   the Tack version, and the platform/architecture. **No code, no file
   paths, no prompt or tool-output content, no environment details beyond
   the OS family.** Payload contents must be documented and inspectable.
3. **Open server side.** The receiving endpoint's server software must be
   open source, so users can verify exactly what is stored and how.
4. **Revocable at any time.** The user can turn it off at any moment, and
   turning it off must stop all transmission immediately.

Until such a mechanism exists and meets all four conditions, the answer to
"does Tack phone home?" remains simply: **no.**
