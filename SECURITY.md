# Security Policy

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub
issues.**

Report them privately through GitHub Security Advisories (private
vulnerability reporting):

<https://github.com/sufar/tack/security/advisories/new>

Include as much detail as you can: affected version(s), platform, a
description of the issue and its impact, and steps to reproduce or a
proof-of-concept if available.

## Scope

Tack is a coding agent that executes commands, talks to LLM providers, and
loads third-party extensions, so the following classes of issues are
especially security-relevant (examples, not an exhaustive list):

- **Sandbox escapes**: breaking out of the bash tool sandbox (macOS
  seatbelt, Linux bubblewrap, Windows `windows-sys`-based restrictions).
- **Credential disclosure**: leakage of OAuth/API credentials stored in
  `~/.tack/agent/auth.json`, or credentials being sent to unintended
  endpoints or logged.
- **Extension protocol injection**: the `tack-ext` subprocess protocol
  (NDJSON JSON-RPC) and the `tack-ext-wasm` wasmtime sandbox being abused to
  gain unintended privileges, as well as MCP client trust issues.
- **Permission system bypasses**: circumventing `permissions.json` to run
  tools or commands the user did not authorize.
- **Supply chain**: malicious or compromised dependencies, build scripts,
  or release artifacts.

If you are unsure whether something is in scope, report it privately anyway —
we would rather triage a non-issue than miss a real one.

## Supported versions

Only the **latest release** of Tack is supported with security fixes. Fixes
ship in the next release; we do not backport patches to older versions.
Please upgrade to the newest release before verifying or reporting issues.

## Response expectations

Tack is maintained by volunteers on a best-effort basis. We will
acknowledge reports and work on a fix as quickly as we can, but we cannot
guarantee specific response or fix timelines. Thank you for your patience
and for helping keep users safe.

## Disclosure policy

We follow coordinated disclosure:

1. You report the vulnerability privately (see above).
2. We investigate, develop a fix, and prepare a release.
3. Once the fix is released, the advisory is made public.
4. Reporters are credited in the public advisory (unless you prefer to
   remain anonymous).

We ask that you do not disclose the issue publicly until a fix is available.
