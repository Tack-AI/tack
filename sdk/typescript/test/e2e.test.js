// End-to-end: a real plugin over an in-memory stream pair, driven by a
// scripted host peer — the same code path as the stdio carrier.

import { test } from "node:test";
import assert from "node:assert/strict";
import { PassThrough } from "node:stream";

import {
  createPeer,
  plugin,
  textOutput,
  allow,
  deny,
  PeerError,
  PROTOCOL_VERSION,
} from "../src/index.js";

const ERR_POLICY_DENIED = -32001;
const ERR_INVALID_PARAMS = -32602;
const ERR_CAPABILITY_NOT_GRANTED = -32002;

function connect() {
  // peer1: input=a, output=b; peer2: input=b, output=a
  return { a: new PassThrough(), b: new PassThrough() };
}

function initParams() {
  return {
    protocolVersion: PROTOCOL_VERSION,
    host: { name: "tack", version: "test" },
    mode: "tui",
    cwd: "/tmp",
    trusted: true,
    capabilities: { widgets: true, uiDialogs: true },
    config: { severity: "high" },
  };
}

function echoPlugin() {
  return plugin({ name: "test-plugin", version: "1.0.0" }).tool(
    { name: "test.echo", description: "echo", parameters: { type: "object" } },
    async (params) => textOutput(`echo: ${JSON.stringify(params.arguments)}`),
  );
}

test("handshake advertises declared capabilities", async () => {
  const { a, b } = connect();
  const host = createPeer({ input: a, output: b });
  const run = echoPlugin().run({ input: b, output: a });
  const result = await host.call("initialize", initParams());
  assert.equal(result.plugin.name, "test-plugin");
  assert.equal(result.plugin.version, "1.0.0");
  assert.deepEqual(
    result.capabilities.tools.map((t) => t.name),
    ["test.echo"],
  );
  assert.equal(result.capabilities.hooks, undefined);
  await host.call("shutdown");
  await run;
  a.end();
  b.end();
});

test("handshake rejects an incompatible host version", async () => {
  const { a, b } = connect();
  const host = createPeer({ input: a, output: b });
  const run = echoPlugin().run({ input: b, output: a });
  await assert.rejects(
    host.call("initialize", { ...initParams(), protocolVersion: "4.0.0" }),
    (err) => err.code === ERR_INVALID_PARAMS && /unsupported host protocol/.test(err.message),
  );
  b.end();
  await run;
  a.end();
});

test("tools/execute roundtrip and unknown tool", async () => {
  const { a, b } = connect();
  const host = createPeer({ input: a, output: b });
  const run = echoPlugin().run({ input: b, output: a });
  await host.call("initialize", initParams());
  const output = await host.call("tools/execute", {
    name: "test.echo",
    toolCallId: "c-1",
    arguments: { x: 42 },
  });
  assert.equal(output.content[0].text, 'echo: {"x":42}');
  await assert.rejects(
    host.call("tools/execute", { name: "nope", toolCallId: "c-2", arguments: {} }),
    (err) => err.code === ERR_INVALID_PARAMS,
  );
  await host.call("shutdown");
  await run;
  a.end();
  b.end();
});

test("beforeToolCall verdicts + capability gating", async () => {
  const { a, b } = connect();
  const host = createPeer({ input: a, output: b });
  const run = plugin({ name: "guard" })
    .beforeToolCall(async (params) =>
      params.toolCall.toolName === "bash" ? deny("no shell today") : allow(),
    )
    .run({ input: b, output: a });
  await host.call("initialize", initParams());
  const call = (toolName) =>
    host.call("hooks/beforeToolCall", {
      toolCall: { toolCallId: "c-1", toolName, arguments: {} },
    });
  assert.deepEqual(await call("bash"), { action: "deny", reason: "no shell today" });
  assert.deepEqual(await call("read"), { action: "allow" });
  await host.call("shutdown");
  await run;
  a.end();
  b.end();

  // A plugin without the hook answers ERR_CAPABILITY_NOT_GRANTED.
  const pair = connect();
  const host2 = createPeer({ input: pair.a, output: pair.b });
  const run2 = echoPlugin().run({ input: pair.b, output: pair.a });
  await host2.call("initialize", initParams());
  await assert.rejects(
    host2.call("hooks/beforeToolCall", {
      toolCall: { toolCallId: "c-1", toolName: "bash", arguments: {} },
    }),
    (err) => err.code === ERR_CAPABILITY_NOT_GRANTED,
  );
  await host2.call("shutdown");
  await run2;
  pair.a.end();
  pair.b.end();
});

test("transformContext null passes through as null result", async () => {
  const { a, b } = connect();
  const host = createPeer({ input: a, output: b });
  const run = plugin({ name: "ctx" })
    .transformContext(async () => null)
    .run({ input: b, output: a });
  await host.call("initialize", initParams());
  const result = await host.call("hooks/transformContext", { messages: [] });
  assert.equal(result, null);
  await host.call("shutdown");
  await run;
  a.end();
  b.end();
});

test("events and widget actions dispatch", async () => {
  const { a, b } = connect();
  const host = createPeer({ input: a, output: b });
  const seen = [];
  const run = plugin({ name: "observer" })
    .events(["turnStart"], async (params) => seen.push(params.event))
    .widget({ id: "list", type: "listPanel", title: "items" })
    .onWidgetAction(async (params) => seen.push(`${params.action}:${params.itemId}`))
    .run({ input: b, output: a });
  const init = await host.call("initialize", initParams());
  assert.deepEqual(init.capabilities.events, ["turnStart"]);
  assert.equal(init.capabilities.widgets.length, 1);
  await host.notify("events/lifecycle", { event: "turnStart", payload: {} });
  await host.notify("widgets/action", { id: "list", action: "select", itemId: "a.rs" });
  for (let i = 0; i < 50 && seen.length < 2; i++) {
    await new Promise((r) => setTimeout(r, 20));
  }
  assert.deepEqual(seen, ["turnStart", "select:a.rs"]);
  await host.call("shutdown");
  await run;
  a.end();
  b.end();
});

test("plugin calls host services from a tool; policy codes survive", async () => {
  const { a, b } = connect();
  const hostRequests = [];
  const host = createPeer({
    input: a,
    output: b,
    handler: {
      async handleRequest(method, params) {
        hostRequests.push(method);
        if (method === "ui/select") return "b";
        if (method === "exec/run") throw new PeerError(ERR_POLICY_DENIED, "exec requires project trust");
        throw new PeerError(-32601, `unknown ${method}`);
      },
    },
  });
  const run = plugin({ name: "needy" })
    .tool(
      { name: "test.ask", description: "uses host services", parameters: { type: "object" } },
      async (_params, cx) => {
        const picked = await cx.host.select("pick", ["a", "b"]);
        const execError = await cx.host.exec("rm -rf /").catch((err) => err);
        assert.equal(execError.code, ERR_POLICY_DENIED);
        return textOutput(`picked=${picked} severity=${cx.config.severity}`);
      },
    )
    .run({ input: b, output: a });
  await host.call("initialize", initParams());
  const output = await host.call("tools/execute", {
    name: "test.ask",
    toolCallId: "c-1",
    arguments: {},
  });
  assert.equal(output.content[0].text, "picked=b severity=high");
  assert.ok(hostRequests.includes("ui/select"));
  assert.ok(hostRequests.includes("exec/run"));
  await host.call("shutdown");
  await run;
  a.end();
  b.end();
});
