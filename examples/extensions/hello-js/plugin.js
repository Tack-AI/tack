// hello-js: a tack extension (tack-ext protocol, NDJSON over stdio).
//
// Registers one tool (echo) and one slash command (/hello-js), listens for
// agent_start, and demonstrates a ui.select dialog and tool_call
// interception. On protocol >= 2 hosts it also demonstrates the v2.1/v2.2
// declarative surface: a status-line segment, a list panel, and a
// '#'-trigger autocomplete provider. Copy this directory to
// ~/.tack/agent/extensions/hello-js/ to install, then restart tack.

import readline from "node:readline";

const rl = readline.createInterface({ input: process.stdin, terminal: false });
let nextId = 1;

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}
function request(method, params) {
  return new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    send({ type: "request", id, method, params });
  });
}
const pending = new Map();

rl.on("line", async (line) => {
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
      // Declarative UI (widgets / autocomplete providers) requires a v2
      // host; v1 hosts ignore unknown register fields, but a well-behaved
      // plugin gates on the negotiated protocol version.
      const v2 = (msg.payload?.protocol ?? 1) >= 2;
      // Handshake: declare our tools/commands/subscriptions.
      send({
        type: "event",
        event: "register",
        payload: {
          name: "hello-js",
          tools: [
            {
              name: "echo",
              description: "Echo the text argument back",
              parameters: {
                type: "object",
                properties: { text: { type: "string" } },
                required: ["text"],
              },
            },
          ],
          commands: [{ name: "hello-js", description: "Greet from JS" }],
          subscriptions: ["agent_start", "tool_call"],
          // v2.1: a status-line segment + a list panel.
          ...(v2 && {
            widgets: [
              {
                id: "hello-status",
                type: "status_line_segment",
                priority: 50,
                initial: { text: "hello-js ⚡", style: "dim" },
              },
              {
                id: "greet-list",
                type: "list_panel",
                title: "Greet someone (hello-js)",
                visible: true,
                initial: {
                  items: [
                    { id: "world", label: "World", detail: "the default" },
                    { id: "tack", label: "tack", detail: "this host" },
                    { id: "wasm", label: "WASM sandbox", detail: "v2 carrier" },
                  ],
                },
              },
            ],
            // v2.2: a '#'-trigger autocomplete provider.
            autocompleteProviders: [
              { id: "names", trigger: "#", description: "Greeting names" },
            ],
          }),
        },
      });
    } else if (msg.event === "agent_start") {
      await request("ui.set_status", { text: "⚡ hello-js watching" });
    } else if (msg.event === "widget.action") {
      // v2.1: the user picked an item in our list panel.
      const itemId = msg.payload?.itemId ?? "anonymous";
      await request("ui.notify", {
        message: `Hello, ${itemId}! (hello-js list pick)`,
      });
      // Push a full-state update to the status segment (idempotent).
      send({
        type: "event",
        event: "widget.update",
        payload: {
          id: "hello-status",
          state: { text: `greeted ${itemId}`, style: "info" },
        },
      });
    } else if (msg.event === "shutdown") {
      process.exit(0);
    }
    return;
  }

  if (msg.type === "request") {
    const { id, method, params } = msg;
    try {
      if (method === "tool.execute") {
        send({
          type: "response",
          id,
          result: { content: `echo: ${params.arguments?.text ?? ""}` },
        });
      } else if (method === "command.invoke") {
        const name = await request("ui.input", {
          title: "Who should hello-js greet?",
          placeholder: "name",
        });
        await request("ui.notify", {
          message: `Hello, ${name ?? "anonymous"}! (from hello-js)`,
        });
        send({ type: "response", id, result: { ok: true } });
      } else if (method === "autocomplete.provide") {
        // v2.2: the input line carries a '#' token; suggest names.
        const query = (params?.query ?? "").toLowerCase();
        const names = ["world", "tack", "wasm", "extensions", "sandboxes"];
        send({
          type: "response",
          id,
          result: {
            suggestions: names
              .filter((n) => !query || n.includes(query))
              .map((n) => ({
                value: `#${n}`,
                label: `#${n}`,
                detail: "hello-js name",
                insertText: `#${n} `,
              })),
          },
        });
      } else if (method === "intercept.tool_call") {
        // Guard example: block destructive bash commands.
        const command = params.arguments?.command ?? "";
        if (params.toolName === "bash" && /rm\s+-rf\s+\/(?:\s|$)/.test(command)) {
          send({
            type: "response",
            id,
            result: { action: "deny", reason: "hello-js blocked a destructive command" },
          });
        } else {
          send({ type: "response", id, result: { action: "allow" } });
        }
      } else {
        send({ type: "response", id, error: `unknown method ${method}` });
      }
    } catch (error) {
      send({ type: "response", id, error: String(error) });
    }
  }
});
