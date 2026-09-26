// git-checkpoint: snapshot the worktree at every turn via `git stash create`
// (never mutates the working tree). `/checkpoints` lists snapshots,
// `/checkpoint-restore <n>` applies one back.
// Install: tack ext install <this-dir>, or copy to ~/.tack/agent/extensions/.

import readline from "node:readline";
import { execFile } from "node:child_process";
import { promisify } from "node:util";

const run = promisify(execFile);
const rl = readline.createInterface({ input: process.stdin, terminal: false });
const send = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");

let nextId = 1;
const pending = new Map();
function request(method, params) {
  return new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    send({ type: "request", id, method, params });
  });
}

const checkpoints = []; // { ref, at, subject }
let gitAvailable = undefined;

async function git(...args) {
  const { stdout } = await run("git", args, { timeout: 10_000 });
  return stdout.trim();
}

async function createCheckpoint() {
  if (gitAvailable === false) return;
  try {
    const ref = await git("stash", "create");
    if (!ref) {
      gitAvailable = false;
      return; // not a git repo
    }
    gitAvailable = true;
    const subject = await git("log", "-1", "--format=%s").catch(() => "");
    checkpoints.push({ ref, at: new Date().toISOString(), subject });
  } catch {
    gitAvailable = false;
  }
}

function listText() {
  if (!checkpoints.length) return "no checkpoints yet";
  return checkpoints
    .map((c, i) => `${i + 1}. [${c.at.slice(0, 19)}] ${c.ref.slice(0, 10)} ${c.subject}`)
    .join("\n");
}

rl.on("line", (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }

  if (msg.type === "response") {
    const p = pending.get(msg.id);
    if (p) {
      pending.delete(msg.id);
      msg.error ? p.reject(new Error(msg.error)) : p.resolve(msg.result);
    }
    return;
  }

  if (msg.type === "event") {
    if (msg.event === "initialize") {
      send({
        type: "event",
        event: "register",
        payload: {
          name: "git-checkpoint",
          commands: [
            { name: "checkpoints", description: "List git checkpoints taken this session" },
            { name: "checkpoint-restore", description: "Apply checkpoint N back to the worktree" },
          ],
          subscriptions: ["turn_start"],
        },
      });
    } else if (msg.event === "turn_start") {
      void createCheckpoint();
    } else if (msg.event === "shutdown") {
      process.exit(0);
    }
    return;
  }

  if (msg.type === "request" && msg.method === "command.invoke") {
    void (async () => {
      try {
        const name = msg.params.name;
        if (name === "checkpoints") {
          await request("ui.notify", { message: listText() });
        } else if (name === "checkpoint-restore") {
          const n = Number((msg.params.args ?? "").trim());
          const cp = checkpoints[n - 1];
          if (!cp) {
            await request("ui.notify", { level: "warning", message: `no checkpoint #${msg.params.args}` });
          } else {
            const ok = await request("ui.confirm", {
              title: "Restore checkpoint",
              message: `git stash apply ${cp.ref.slice(0, 10)} (${cp.subject})?`,
            });
            if (ok) {
              try {
                await git("stash", "apply", cp.ref);
                await request("ui.notify", { message: `restored checkpoint #${n}` });
              } catch (e) {
                await request("ui.notify", { level: "error", message: `restore failed: ${e}` });
              }
            }
          }
        }
        send({ type: "response", id: msg.id, result: { ok: true } });
      } catch (e) {
        send({ type: "response", id: msg.id, error: String(e) });
      }
    })();
    return;
  }

  if (msg.type === "request") {
    send({ type: "response", id: msg.id, error: `unknown method ${msg.method}` });
  }
});
