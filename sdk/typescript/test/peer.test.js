// Peer-level robustness: write failures are terminal (no unhandled
// rejections, no poisoned write chain) and non-envelope lines are
// ignored — matching the Rust and Python peers.

import { test } from "node:test";
import assert from "node:assert/strict";
import { PassThrough } from "node:stream";

import { createPeer, PeerError } from "../src/index.js";

const ERR_PLUGIN_UNAVAILABLE = -32003;
const ERR_REQUEST_TIMEOUT = -32004;

const tick = () => new Promise((resolve) => setImmediate(resolve));

function sinkOutput() {
  const lines = [];
  return {
    lines,
    write(s, cb) {
      lines.push(s);
      cb();
    },
  };
}

test("a write failure while responding marks the peer dead instead of crashing", async () => {
  const input = new PassThrough();
  let failWrites = false;
  const output = {
    write(_s, cb) {
      if (failWrites) cb(new Error("boom"));
      else cb();
    },
  };
  const peer = createPeer({
    input,
    output,
    handler: { handleRequest: async () => "ok" },
  });

  // The response to this request fails on the wire (host killed the
  // pipe while the response was in flight).
  failWrites = true;
  input.write(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "m", params: null }) + "\n");
  for (let i = 0; i < 5; i++) await tick();

  assert.equal(peer.alive, false, "write failure is terminal");
  // Later calls fail fast with the dead-peer error, not the stale
  // first write error (the write chain is not poisoned).
  await assert.rejects(peer.call("later"), (err) => {
    assert.equal(err.code, ERR_PLUGIN_UNAVAILABLE);
    assert.match(err.message, /peer is unavailable/);
    return true;
  });
  await assert.rejects(peer.notify("later"), /peer is unavailable/);
});

test("a failed outgoing write marks the peer dead and later calls fail fast", async () => {
  const input = new PassThrough();
  let calls = 0;
  const output = {
    write(_s, cb) {
      calls += 1;
      cb(new Error("boom"));
    },
  };
  const peer = createPeer({ input, output });

  await assert.rejects(peer.call("m"), (err) => {
    assert.equal(err.code, ERR_PLUGIN_UNAVAILABLE);
    assert.match(err.message, /write failed/);
    return true;
  });
  assert.equal(peer.alive, false);
  const before = calls;
  await assert.rejects(peer.call("m2"), /peer is unavailable/);
  assert.equal(calls, before, "no further writes are attempted on a dead peer");
});

test("a bare null line (and other non-envelope values) are ignored", async () => {
  const input = new PassThrough();
  const output = sinkOutput();
  const peer = createPeer({
    input,
    output,
    handler: { handleRequest: async (method) => `pong:${method}` },
  });

  input.write("null\n");
  input.write("[1, 2, 3]\n");
  input.write('"just a string"\n');
  input.write("42\n");
  for (let i = 0; i < 3; i++) await tick();
  assert.equal(peer.alive, true, "non-envelope lines do not kill the peer");
  assert.equal(output.lines.length, 0, "no error responses for ignored values");

  // The peer still answers requests afterwards.
  input.write(JSON.stringify({ jsonrpc: "2.0", id: 7, method: "ping" }) + "\n");
  for (let i = 0; i < 5 && output.lines.length === 0; i++) await tick();
  assert.equal(peer.alive, true);
  const response = JSON.parse(output.lines[0]);
  assert.equal(response.id, 7);
  assert.equal(response.result, "pong:ping");
});

test("a destructured call() still sends $/cancelRequest on timeout", async () => {
  const input = new PassThrough();
  const output = sinkOutput();
  const peer = createPeer({ input, output });

  const { call } = peer; // no receiver: must not rely on `this`
  await assert.rejects(call("never-answered", null, 20), (err) => {
    assert.ok(err instanceof PeerError);
    assert.equal(err.code, ERR_REQUEST_TIMEOUT);
    return true;
  });
  const cancel = output.lines.map((l) => JSON.parse(l)).find((m) => m.method === "$/cancelRequest");
  assert.ok(cancel, "timeout notifies the peer via $/cancelRequest");
});
