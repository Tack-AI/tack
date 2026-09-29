// Provider bridge e2e tests (P7): a plugin serving inference over an
// in-memory stream pair, driven by a scripted host peer — the same code
// path as the stdio carrier. Mirrors crates/tack-ext-sdk/tests/provider_e2e.rs.

import { test } from "node:test";
import assert from "node:assert/strict";
import { PassThrough } from "node:stream";

import { createPeer, plugin, PeerError, PROTOCOL_VERSION } from "../src/index.js";

const ERR_CAPABILITY_NOT_GRANTED = -32002;

function connect() {
  // host peer: input=a, output=b; plugin: input=b, output=a
  return { a: new PassThrough(), b: new PassThrough() };
}

function initParams() {
  return {
    protocolVersion: PROTOCOL_VERSION,
    host: { name: "tack", version: "test" },
    mode: "tui",
    cwd: "/tmp",
    trusted: true,
    capabilities: { providerRegistration: true },
    config: null,
  };
}

/** Host stub: captures provider registrations and stream events. */
function makeHost(a, b) {
  const stub = { registrations: [], streamEvents: [] };
  const peer = createPeer({
    input: a,
    output: b,
    handler: {
      async handleRequest(method, params) {
        if (method === "host/registerProvider") {
          stub.registrations.push(params.provider ?? null);
          return null;
        }
        throw new PeerError(-32601, `stub: unknown ${method}`);
      },
      async handleNotification(method, params) {
        if (method === "provider/streamEvent") {
          stub.streamEvents.push([params.streamId, params.event]);
        }
      },
    },
  });
  return { peer, stub };
}

function streamParams(text) {
  return {
    streamId: "ps-test-1",
    model: { id: "fake-1", provider: "demo-provider", api: "ext-provider-bridge" },
    context: { messages: [{ role: "user", content: text, timestamp: 0 }] },
    options: { maxTokens: 1024 },
  };
}

/** A pending assistant message skeleton for scripted events. */
function partial(model) {
  return {
    content: [],
    api: model.api ?? "",
    provider: model.provider ?? "",
    model: model.id ?? "",
    usage: {
      input: 0,
      output: 0,
      cacheRead: 0,
      cacheWrite: 0,
      totalTokens: 0,
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
    },
    stopReason: "pending",
    timestamp: 0,
  };
}

async function waitForEvents(stub, count) {
  for (let i = 0; i < 100; i++) {
    if (stub.streamEvents.length >= count) return stub.streamEvents;
    await new Promise((r) => setTimeout(r, 20));
  }
  assert.fail(`timed out waiting for ${count} stream events`);
}

async function waitFor(list, count) {
  for (let i = 0; i < 100; i++) {
    if (list.length >= count) return;
    await new Promise((r) => setTimeout(r, 20));
  }
  assert.fail(`timed out waiting for ${count} items`);
}

async function finish(host, run, a, b) {
  await host.call("shutdown");
  await run;
  a.end();
  b.end();
}

function echoPlugin() {
  return plugin({ name: "provider-plugin" }).providerStream(async (params, events) => {
    await events.send({ type: "start", partial: partial(params.model) });
    await events.textDelta(0, "hello", partial(params.model));
    const message = partial(params.model);
    message.content = [{ type: "text", text: "hello" }];
    message.stopReason = "stop";
    await events.done(message);
  });
}

test("capability advertised and stream events flow", async () => {
  const { a, b } = connect();
  const { peer: host, stub } = makeHost(a, b);
  const run = echoPlugin().run({ input: b, output: a });
  const init = await host.call("initialize", initParams());
  assert.equal(init.capabilities.provider.stream, true);
  const ack = await host.call("provider/stream", streamParams("hi"));
  assert.equal(ack, null, "fast null ack");
  const captured = await waitForEvents(stub, 3);
  assert.ok(captured.every(([id]) => id === "ps-test-1"));
  assert.equal(captured[0][1].type, "start");
  assert.equal(captured[1][1].type, "textDelta");
  assert.equal(captured[1][1].delta, "hello");
  assert.equal(captured[2][1].type, "done");
  assert.equal(captured[2][1].message.stopReason, "stop");
  await finish(host, run, a, b);
});

test("onReady registers the provider", async () => {
  const { a, b } = connect();
  const { peer: host, stub } = makeHost(a, b);
  const run = plugin({ name: "provider-plugin" })
    .providerStream(async () => {})
    .onReady(async (cx) => {
      await cx.host.registerProvider({
        id: "demo-provider",
        bridge: true,
        models: [{ id: "fake-1" }],
      });
    })
    .run({ input: b, output: a });
  await host.call("initialize", initParams());
  await waitFor(stub.registrations, 1);
  assert.equal(stub.registrations.length, 1);
  assert.equal(stub.registrations[0].id, "demo-provider");
  assert.equal(stub.registrations[0].bridge, true);
  await finish(host, run, a, b);
});

test("missing terminal fires the automatic error", async () => {
  const { a, b } = connect();
  const { peer: host, stub } = makeHost(a, b);
  const run = plugin({ name: "provider-plugin" })
    .providerStream(async (params, events) => {
      await events.send({ type: "start", partial: partial(params.model) });
      // no terminal: SDK enforcement fires
    })
    .run({ input: b, output: a });
  await host.call("initialize", initParams());
  await host.call("provider/stream", streamParams("hi"));
  const captured = await waitForEvents(stub, 2);
  assert.equal(captured[0][1].type, "start");
  assert.equal(captured[1][1].type, "error");
  assert.match(captured[1][1].error.errorMessage, /without a terminal event/);
  // The synthesized error message is a valid assistant message shape.
  assert.equal(captured[1][1].error.provider, "demo-provider");
  assert.equal(captured[1][1].error.model, "fake-1");
  assert.equal(captured[1][1].error.api, "ext-provider-bridge");
  assert.equal(captured[1][1].error.stopReason, "error");
  await finish(host, run, a, b);
});

test("handler error becomes the terminal error event", async () => {
  const { a, b } = connect();
  const { peer: host, stub } = makeHost(a, b);
  const run = plugin({ name: "provider-plugin" })
    .providerStream(async () => {
      throw new Error("backend exploded");
    })
    .run({ input: b, output: a });
  await host.call("initialize", initParams());
  await host.call("provider/stream", streamParams("hi"));
  const captured = await waitForEvents(stub, 1);
  assert.equal(captured[0][1].type, "error");
  assert.equal(captured[0][1].error.errorMessage, "backend exploded");
  await finish(host, run, a, b);
});

test("second terminal event is rejected", async () => {
  const { a, b } = connect();
  const { peer: host, stub } = makeHost(a, b);
  const run = plugin({ name: "provider-plugin" })
    .providerStream(async (params, events) => {
      const message = partial(params.model);
      message.stopReason = "stop";
      await events.done(message);
      await assert.rejects(events.done(message), /already terminated/);
    })
    .run({ input: b, output: a });
  await host.call("initialize", initParams());
  await host.call("provider/stream", streamParams("hi"));
  const captured = await waitForEvents(stub, 1);
  await new Promise((r) => setTimeout(r, 100));
  assert.equal(captured.length, 1, "exactly one terminal crossed the wire");
  assert.equal(captured[0][1].type, "done");
  await finish(host, run, a, b);
});

test("stream cancel reaches the handler", async () => {
  const { a, b } = connect();
  const { peer: host, stub } = makeHost(a, b);
  const run = plugin({ name: "provider-plugin" })
    .providerStream(async (params, events, cx) => {
      await events.send({ type: "start", partial: partial(params.model) });
      await cx.cancelled();
      assert.equal(cx.isCancelled(), true);
      const error = partial(params.model);
      error.stopReason = "aborted";
      error.errorMessage = "demo aborted";
      await events.send({ type: "error", reason: "aborted", error });
    })
    .run({ input: b, output: a });
  await host.call("initialize", initParams());
  await host.call("provider/stream", streamParams("hi"));
  await waitForEvents(stub, 1); // start landed
  await host.notify("provider/streamCancel", { streamId: "ps-test-1" });
  const captured = await waitForEvents(stub, 2);
  assert.equal(captured[1][1].type, "error");
  assert.equal(captured[1][1].reason, "aborted");
  // Unknown stream ids are ignored.
  await host.notify("provider/streamCancel", { streamId: "nope" });
  await finish(host, run, a, b);
});

test("undeclared providerStream is capability-not-granted", async () => {
  const { a, b } = connect();
  const { peer: host } = makeHost(a, b);
  const run = plugin({ name: "plain-plugin" }).run({ input: b, output: a });
  const init = await host.call("initialize", initParams());
  assert.equal(init.capabilities.provider, undefined);
  await assert.rejects(
    host.call("provider/stream", streamParams("hi")),
    (err) => err.code === ERR_CAPABILITY_NOT_GRANTED,
  );
  await finish(host, run, a, b);
});

test("options and context arrive verbatim", async () => {
  const { a, b } = connect();
  const { peer: host, stub } = makeHost(a, b);
  const run = plugin({ name: "provider-plugin" })
    .providerStream(async (params, events) => {
      assert.equal(params.options.maxTokens, 1024);
      assert.equal(params.context.messages[0].content, "check");
      assert.equal(params.streamId, "ps-test-1");
      assert.equal(events.streamId, "ps-test-1");
      await events.error("seen");
    })
    .run({ input: b, output: a });
  await host.call("initialize", initParams());
  await host.call("provider/stream", streamParams("check"));
  const captured = await waitForEvents(stub, 1);
  assert.equal(captured[0][1].error.errorMessage, "seen");
  await finish(host, run, a, b);
});
