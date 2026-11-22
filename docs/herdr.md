# Herdr integration

**English | [简体中文](herdr.zh-CN.md)**

Tack has a built-in client for [Herdr](https://herdr.dev), the terminal
workspace manager for coding agents. Run `tack` in a Herdr pane and it appears
as a first-class `tack` agent — no Herdr fork, plugin, integration installer,
or Tack setting is required.

## What works

- **Lifecycle state:** Herdr shows `working`, `idle`, or `blocked` for the
  pane. Permission questions, `ask_user`, MCP elicitation, and agent-related
  extension dialogs count as blocked.
- **Automation and notifications:** the reported state works with Herdr's
  sidebar, notifications, `herdr agent list`, and `herdr agent wait`.
- **Session restore:** for a persistent session, Tack reports its session ID
  and a `tack --agent-dir <dir> --session <id> --continue --session-dir <dir>
  --provider <provider> --model <model>` resume command (plus `--thinking`
  when set). Herdr can
  reopen that session in the same pane and working directory after a server
  restart, with its transcript replayed. Session changes made with commands
  such as `/new` and `/resume` are reported too. Provider, model, or
  thinking-level changes update the resume command as well.
- **Clean exit:** Tack releases its Herdr pane authority when the TUI exits.
  Herdr's shell-prompt detection remains a safety net if a report cannot be
  delivered.

Only the interactive TUI reports to Herdr. Print, JSONL RPC, ACP, remote, and
nested headless processes do not take over the pane merely because they
inherit Herdr environment variables.

## Use it

Install the official Herdr release (Herdr 0.9.2 or newer is required for
session-resume commands), then start it where the project lives:

```sh
brew install herdr       # or use the installer on https://herdr.dev
cd /path/to/project
herdr
```

Run `tack` in a pane. The `tack` executable must be on `PATH` if you want
Herdr to restore sessions after a server restart.

Useful checks from another pane:

```sh
herdr agent list
herdr pane get <pane-id>
herdr agent wait <agent-or-pane>
```

To test restore without affecting your normal Herdr session:

```sh
herdr --session tack-test
# Start tack in the test pane, then from another terminal:
herdr session stop tack-test
herdr --session tack-test
```

`--no-session` sessions still report lifecycle state, but they have no
persistent session to restore. If a session ID or directory cannot be
represented in Herdr's resume-command format (for example, it contains an
apostrophe or a control character), Tack still reports the session identity
but omits the invalid resume command.

## Activation and opting out

Herdr supplies the pane environment; do not set these variables yourself:

| Variable | Purpose |
|---|---|
| `HERDR_ENV=1` | Opts the interactive TUI into reporting |
| `HERDR_PANE_ID` | Identifies the pane that owns this Tack process |
| `HERDR_BIN_PATH` | Herdr executable used for portable CLI reports |
| `HERDR_SOCKET_PATH` | Herdr server endpoint inherited by that CLI |

Reporting is best-effort and asynchronous: repeated states are coalesced,
each command has a short timeout, and Herdr failures never block or fail the
agent loop. Reports use monotonically increasing sequence numbers so an older
report cannot overwrite newer state.

To run the TUI inside Herdr without reporting, remove the opt-in variable for
that process:

```sh
# Unix
env -u HERDR_ENV tack
```

```powershell
# Windows PowerShell
Remove-Item Env:HERDR_ENV
tack
```

Use the normal Herdr upgrade path. There is no Tack-specific code installed
inside Herdr and no generated file to reinstall after either program updates.

## Reported data

Tack reports only the agent name, lifecycle state, a generic waiting-for-input
message, the persistent session ID, and the agent/session directories,
provider, model, and thinking level needed for the resume command. Prompts,
responses, tool arguments, credentials, and file contents are not included in
Herdr reports.
