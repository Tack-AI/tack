// handoff: `/handoff <goal>` — package the current conversation tail plus a
// goal into a fresh session (TS handoff.ts equivalent). The plugin tracks
// recent messages from message_end events, then uses session control to
// start a new session with the packaged context.
// Install: tack ext install <this-dir>, or copy to ~/.tack/agent/extensions/.

import readline from "node:readline";

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

const MAX_TAIL = 6;
const MAX_CHARS = 4000;
const recent = []; // {role, text}

function pushRecent(role, text) {
  if (!text || !text.trim()) return;
  recent.push({ role, text: text.trim() });
  while (recent.length > MAX_TAIL) recent.shift();
}

function packageContext(goal) {
  const tail = recent
    .map((m) => `[${m.role}] ${m.text}`)
    .join("\n\n")
    .slice(-MAX_CHARS);
  return [
    `You are continuing from a previous session. Your goal: ${goal}`,
    tail ? `\nRelevant context from the previous session:\n\n${tail}` : "",
    "\nAcknowledge briefly, then continue working on the goal.",
  ].join("\n");
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
          name: "handoff",
          commands: [{ name: "handoff", description: "Move to a focused session with the current context and a goal" }],
          subscriptions: ["message_end", "session_start"],
        },
      });
    } else if (msg.event === "message_end") {
      const message = msg.payload?.message;
      if (message?.role === "assistant" && typeof message.content === "object") {
        const text = (message.content ?? [])
          .filter((b) => b.type === "text")
          .map((b) => b.text ?? "")
          .join("\n");
        pushRecent("assistant", text);
      } else if (message?.role === "user") {
        const content = message.content;
        const text = typeof content === "string"
          ? content
          : (content ?? []).filter((b) => b.type === "text").map((b) => b.text ?? "").join(" ");
        pushRecent("user", text);
      }
    } else if (msg.event === "session_start") {
      recent.length = 0;
    } else if (msg.event === "shutdown") {
      process.exit(0);
    }
    return;
  }

  if (msg.type === "request" && msg.method === "command.invoke") {
    void (async () => {
      try {
        let goal = (msg.params.args ?? "").trim();
        if (!goal) {
          goal = await request("ui.input", {
            title: "Handoff goal",
            placeholder: "what should the new session focus on?",
          });
          goal = (goal ?? "").trim();
        }
        if (!goal) {
          await request("ui.notify", { level: "warning", message: "handoff cancelled (no goal)" });
        } else {
          await request("session.new", {});
          await request("session.send_user_message", { text: packageContext(goal) });
          await request("ui.notify", { message: `handed off to a new session (goal: ${goal})` });
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
