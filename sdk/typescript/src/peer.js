// JsonRpcPeer: transport-agnostic JSON-RPC 2.0 peer for tack-RPC v3.
// Input is any async-iterable of chunks (a Node stream); output is any
// writable with .write(string, callback). Both sides may issue requests
// concurrently; ids are per-sender.

export const JSONRPC_VERSION = "2.0";
export const CANCEL_METHOD = "$/cancelRequest";

export const ERR_PARSE = -32700;
export const ERR_METHOD_NOT_FOUND = -32601;
export const ERR_INTERNAL = -32603;
export const ERR_PLUGIN_UNAVAILABLE = -32003;
export const ERR_REQUEST_TIMEOUT = -32004;

/** An error raised by a failed outgoing call. `code` is the JSON-RPC
 * error code (domain codes like ERR_POLICY_DENIED survive the trip). */
export class PeerError extends Error {
  constructor(code, message, data) {
    super(message);
    this.code = code;
    this.data = data;
  }
}

const DEFAULT_REQUEST_TIMEOUT_MS = 30_000;
const MAX_LINE_BYTES = 16 * 1024 * 1024;

/**
 * @param {object} options
 * @param {AsyncIterable<Buffer|string>} options.input  chunk source
 * @param {{ write: (s: string, cb?: (err?: Error) => void) => unknown }} options.output
 * @param {object} [options.handler]
 * @param {(method: string, params: any) => Promise<any>} [options.handler.handleRequest]
 * @param {(method: string, params: any) => Promise<void>} [options.handler.handleNotification]
 */
export function createPeer({ input, output, handler = {} }) {
  const pending = new Map(); // id -> { resolve, reject, timer }
  const inflight = new Map(); // id -> AbortController (incoming requests)
  let nextId = 1;
  let alive = true;
  let writeChain = Promise.resolve();

  const handleRequest = handler.handleRequest ??
    ((method) => Promise.reject(new PeerError(ERR_METHOD_NOT_FOUND, `unknown method ${method}`)));
  const handleNotification = handler.handleNotification ?? (() => Promise.resolve());

  function writeLine(line) {
    if (!alive) {
      return Promise.reject(new PeerError(ERR_PLUGIN_UNAVAILABLE, "peer is unavailable"));
    }
    // Serialize writes (a stream is not concurrent-write safe). A failed
    // write is terminal (the Rust peer's Dead semantics): markDead below,
    // and keep writeChain itself usable so it never re-rejects later
    // writes with the stale first error.
    const write = writeChain.then(
      () =>
        new Promise((resolve, reject) => {
          try {
            output.write(line + "\n", (err) => (err ? reject(err) : resolve()));
          } catch (err) {
            reject(err);
          }
        }),
    );
    writeChain = write.catch(() => {});
    return write.catch((err) => {
      // The write failure is terminal: pending waiters see the real
      // cause, later calls fail fast with the dead-peer error.
      markDead(new PeerError(ERR_PLUGIN_UNAVAILABLE, `write failed: ${err.message ?? err}`));
      throw new PeerError(ERR_PLUGIN_UNAVAILABLE, `write failed: ${err.message ?? err}`);
    });
  }

  async function respond(id, result, error) {
    const message = { jsonrpc: JSONRPC_VERSION, id: id ?? null };
    if (error) message.error = { code: error.code ?? ERR_INTERNAL, message: error.message, data: error.data };
    else message.result = result === undefined ? null : result;
    await writeLine(JSON.stringify(message));
  }

  function dispatch(message) {
    // Only JSON-RPC envelopes (non-null objects) are dispatched; bare
    // values like `null` are ignored, matching the Rust/Python peers.
    if (message === null || typeof message !== "object") return;
    const { method, id } = message;
    const params = message.params ?? null;
    if (method !== undefined && id !== undefined && id !== null) {
      // Incoming request: answer (best-effort cancellable).
      const controller = new AbortController();
      inflight.set(JSON.stringify(id), controller);
      const aborted = new Promise((_, reject) => {
        controller.signal.addEventListener("abort", () =>
          reject(new PeerError(ERR_REQUEST_TIMEOUT, "cancelled by peer")),
        );
      });
      Promise.race([Promise.resolve().then(() => handleRequest(method, params)), aborted])
        .then(
          (result) => respond(id, result, null),
          (error) => {
            if (controller.signal.aborted) return; // cancelled: no response
            return respond(id, null, error instanceof PeerError ? error : new PeerError(ERR_INTERNAL, String(error?.message ?? error)));
          },
        )
        // A write failure while responding is terminal: writeLine has
        // already marked the peer dead — just swallow the rejection so
        // it cannot crash the process as an unhandled rejection.
        .catch(() => {})
        .finally(() => inflight.delete(JSON.stringify(id)));
    } else if (method !== undefined) {
      // Incoming notification.
      if (method === CANCEL_METHOD) {
        const key = JSON.stringify(message.params?.id);
        inflight.get(key)?.abort();
        return;
      }
      Promise.resolve(handleNotification(method, params)).catch(() => {});
    } else if (id !== undefined) {
      // Incoming response.
      const entry = pending.get(JSON.stringify(id));
      if (!entry) return;
      pending.delete(JSON.stringify(id));
      clearTimeout(entry.timer);
      if (message.error) {
        entry.reject(new PeerError(message.error.code, message.error.message, message.error.data));
      } else {
        entry.resolve(message.result ?? null);
      }
    }
  }

  function markDead(cause) {
    if (!alive) return;
    alive = false;
    for (const { reject, timer } of pending.values()) {
      clearTimeout(timer);
      reject(cause ?? new PeerError(ERR_PLUGIN_UNAVAILABLE, "peer is unavailable"));
    }
    pending.clear();
    for (const controller of inflight.values()) controller.abort();
    inflight.clear();
  }

  // Read pump: split chunks into NDJSON lines with a hard cap.
  (async () => {
    let buffer = "";
    try {
      for await (const chunk of input) {
        buffer += typeof chunk === "string" ? chunk : chunk.toString("utf8");
        if (buffer.length > MAX_LINE_BYTES && !buffer.includes("\n")) {
          markDead();
          return;
        }
        let index;
        while ((index = buffer.indexOf("\n")) >= 0) {
          const line = buffer.slice(0, index).replace(/\r$/, "");
          buffer = buffer.slice(index + 1);
          if (!line.trim()) continue;
          let message;
          try {
            message = JSON.parse(line);
          } catch {
            await respond(null, null, new PeerError(ERR_PARSE, "parse error"));
            continue;
          }
          dispatch(message);
        }
      }
    } catch {
      // Read failure: fall through to markDead.
    }
    markDead();
  })();

  /** Fire-and-forget notification (internal: safe to call without a
   * receiver, unlike the destructurable method on the returned peer). */
  function notifyMethod(method, params) {
    if (!alive) return Promise.reject(new PeerError(ERR_PLUGIN_UNAVAILABLE, "peer is unavailable"));
    return writeLine(JSON.stringify({ jsonrpc: JSONRPC_VERSION, method, params }));
  }

  return {
    get alive() {
      return alive;
    },

    /** Call a method and await the result. Rejects with PeerError. */
    call(method, params, timeoutMs = DEFAULT_REQUEST_TIMEOUT_MS) {
      if (!alive) return Promise.reject(new PeerError(ERR_PLUGIN_UNAVAILABLE, "peer is unavailable"));
      const id = nextId++;
      const key = JSON.stringify(id);
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          pending.delete(key);
          notifyMethod(CANCEL_METHOD, { id }).catch(() => {});
          reject(new PeerError(ERR_REQUEST_TIMEOUT, `request timed out: ${method}`));
        }, timeoutMs);
        pending.set(key, { resolve, reject, timer });
        writeLine(JSON.stringify({ jsonrpc: JSONRPC_VERSION, id, method, params })).catch((err) => {
          pending.delete(key);
          clearTimeout(timer);
          reject(err);
        });
      });
    },

    /** Fire-and-forget notification. */
    notify: notifyMethod,
  };
}
