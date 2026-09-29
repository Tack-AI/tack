// Plugin builder + dispatcher + serve loop for tack-RPC v3 (process
// carrier: stdin/stdout). Plugin code never sees an envelope.

import { createPeer, PeerError, ERR_METHOD_NOT_FOUND } from "./peer.js";

export const PROTOCOL_VERSION = "3.0.0";
const ERR_INVALID_PARAMS = -32602;
const ERR_CAPABILITY_NOT_GRANTED = -32002;

function protocolCompatible(peerVersion) {
  const parse = (v) => {
    const [major, minor = "0"] = String(v).split(".");
    return [Number(major), Number(minor)];
  };
  const [peerMajor, peerMinor] = parse(peerVersion);
  const [ourMajor, ourMinor] = parse(PROTOCOL_VERSION);
  return peerMajor === ourMajor && peerMinor <= ourMinor;
}

function invalidParams(message) {
  return new PeerError(ERR_INVALID_PARAMS, message);
}

/** A plugin under construction. Every capability is optional; undeclared
 * capabilities cost nothing (the host skips the calls). */
export function plugin({ name, version, description } = {}) {
  const state = {
    name,
    version,
    description,
    tools: new Map(), // name -> { spec, handler }
    commands: new Map(),
    hooks: {},
    events: null,
    eventHandler: null,
    widgets: [],
    widgetActionHandler: null,
    autocomplete: new Map(),
    configSchema: null,
    metrics: null,
    providerStream: null,
    onReady: null,
  };

  const builder = {
    tool(spec, handler) {
      state.tools.set(spec.name, { spec, handler });
      return builder;
    },
    command(name, description, handler) {
      state.commands.set(name, { spec: { name, description: description ?? undefined }, handler });
      return builder;
    },
    beforeToolCall(handler) {
      state.hooks.beforeToolCall = handler;
      return builder;
    },
    afterToolCall(handler) {
      state.hooks.afterToolCall = handler;
      return builder;
    },
    transformContext(handler) {
      state.hooks.transformContext = handler;
      return builder;
    },
    approvalReview(handler) {
      state.hooks.approvalReview = handler;
      return builder;
    },
    events(names, handler) {
      state.events = names;
      state.eventHandler = handler;
      return builder;
    },
    widget(spec) {
      state.widgets.push(spec);
      return builder;
    },
    onWidgetAction(handler) {
      state.widgetActionHandler = handler;
      return builder;
    },
    autocomplete(spec, handler) {
      state.autocomplete.set(spec.id, { spec, handler });
      return builder;
    },
    configSchema(schema) {
      state.configSchema = schema;
      return builder;
    },
    metrics(declaration) {
      state.metrics = declaration;
      return builder;
    },
    /** Serve inference for registered providers (the P7 provider
     * bridge): declares the `provider.stream` capability. The handler
     * receives (params, events, streamCx); the turn's events flow back
     * through `events`, cancellation surfaces on `streamCx`. */
    providerStream(handler) {
      state.providerStream = handler;
      return builder;
    },
    /** Fired once after the initialize handshake is answered (spawned;
     * must not delay the answer). The registration entry point for
     * provider plugins: call `cx.host.registerProvider(...)` here. */
    onReady(handler) {
      state.onReady = handler;
      return builder;
    },

    /** Serve over stdio (default) or a custom transport. Resolves on
     * `shutdown` or when the host closes the transport. */
    async run({ input = process.stdin, output = process.stdout } = {}) {
      let initParams = null;
      let resolveDone;
      const done = new Promise((resolve) => (resolveDone = resolve));

      const cx = () => ({
        mode: initParams?.mode,
        trusted: initParams?.trusted,
        cwd: initParams?.cwd,
        capabilities: initParams?.capabilities ?? {},
        config: initParams?.config,
        host: hostClient,
      });

      // streamId -> cancel for in-flight `provider/stream` handlers
      // (`provider/streamCancel` fires it; entries drop when the
      // handler settles).
      const providerStreams = new Map();

      function onInitialize(params) {
        if (!protocolCompatible(params.protocolVersion)) {
          throw invalidParams(
            `unsupported host protocol ${params.protocolVersion} (this plugin speaks ${PROTOCOL_VERSION})`,
          );
        }
        initParams = params;
        const capabilities = {};
        if (state.tools.size) capabilities.tools = [...state.tools.values()].map((t) => t.spec);
        if (state.commands.size) capabilities.commands = [...state.commands.values()].map((c) => c.spec);
        const hooks = {
          beforeToolCall: state.hooks.beforeToolCall ? true : undefined,
          transformContext: state.hooks.transformContext ? true : undefined,
          afterToolCall: state.hooks.afterToolCall ? true : undefined,
          approvalReview: state.hooks.approvalReview ? true : undefined,
        };
        if (Object.values(hooks).some(Boolean)) capabilities.hooks = hooks;
        if (state.eventHandler) capabilities.events = state.events;
        if (state.widgets.length) capabilities.widgets = state.widgets;
        if (state.autocomplete.size)
          capabilities.autocompleteProviders = [...state.autocomplete.values()].map((a) => a.spec);
        if (state.configSchema) capabilities.config = { schema: state.configSchema };
        if (state.metrics) capabilities.metrics = state.metrics;
        if (state.providerStream) capabilities.provider = { stream: true };
        // The startup hook (provider plugins register their providers
        // here). Spawned: onReady must not delay the handshake answer.
        if (state.onReady) {
          const handler = state.onReady;
          Promise.resolve()
            .then(() => handler(cx()))
            .catch(() => {});
        }
        return {
          protocolVersion: PROTOCOL_VERSION,
          plugin: { name: state.name, version: state.version, description: state.description },
          capabilities,
        };
      }

      async function handleRequest(method, params) {
        switch (method) {
          case "initialize":
            return onInitialize(params);
          case "shutdown":
            resolveDone();
            return null;
          case "tools/execute": {
            const entry = state.tools.get(params.name);
            if (!entry) throw invalidParams(`unknown tool ${JSON.stringify(params.name)}`);
            return entry.handler(params, cx());
          }
          case "commands/invoke": {
            const entry = state.commands.get(params.name);
            if (!entry) throw invalidParams(`unknown command ${JSON.stringify(params.name)}`);
            return entry.handler(params, cx()) ?? null;
          }
          case "hooks/beforeToolCall":
            if (!state.hooks.beforeToolCall) throw notGranted(method);
            return state.hooks.beforeToolCall(params, cx());
          case "hooks/afterToolCall":
            if (!state.hooks.afterToolCall) throw notGranted(method);
            return (await state.hooks.afterToolCall(params, cx())) ?? null;
          case "hooks/transformContext":
            if (!state.hooks.transformContext) throw notGranted(method);
            return (await state.hooks.transformContext(params, cx())) ?? null;
          case "approval/review":
            if (!state.hooks.approvalReview) throw notGranted(method);
            return (await state.hooks.approvalReview(params, cx())) ?? null;
          case "autocomplete/provide": {
            const entry = state.autocomplete.get(params.providerId);
            if (!entry) throw invalidParams(`unknown autocomplete provider ${JSON.stringify(params.providerId)}`);
            return entry.handler(params, cx());
          }
          case "provider/stream": {
            if (!state.providerStream) throw notGranted(method);
            const streamId = params?.streamId;
            if (typeof streamId !== "string" || streamId.length === 0)
              throw invalidParams("provider/stream requires a streamId");
            let cancelled = false;
            let resolveCancel;
            const cancelPromise = new Promise((resolve) => (resolveCancel = resolve));
            providerStreams.set(streamId, () => {
              cancelled = true;
              resolveCancel();
            });
            const events = makeProviderEvents(peer, streamId, params.model);
            const streamCx = {
              cx: cx(),
              streamId,
              isCancelled: () => cancelled,
              cancelled: () => cancelPromise,
            };
            const handler = state.providerStream;
            // The ack is fast: validation is done; the stream rides
            // provider/streamEvent notifications from here on.
            (async () => {
              try {
                await handler(params, events, streamCx);
                await events.enforceTerminal(null);
              } catch (err) {
                await events.enforceTerminal(err);
              } finally {
                providerStreams.delete(streamId);
              }
            })().catch(() => {});
            return null;
          }
          default:
            throw new PeerError(ERR_METHOD_NOT_FOUND, `unknown method ${method}`);
        }
      }

      async function handleNotification(method, params) {
        if (method === "events/lifecycle" && state.eventHandler) {
          await state.eventHandler(params, cx());
        } else if (method === "widgets/action" && state.widgetActionHandler) {
          await state.widgetActionHandler(params, cx());
        } else if (method === "provider/streamCancel") {
          providerStreams.get(params?.streamId)?.();
        }
      }

      const peer = createPeer({ input, output, handler: { handleRequest, handleNotification } });
      const hostClient = makeHostClient(peer);

      // Resolve when the host asks for shutdown OR the transport dies.
      let polling = true;
      const dead = (async () => {
        while (polling && peer.alive) await new Promise((r) => setTimeout(r, 50));
      })();
      await Promise.race([done, dead]);
      polling = false;
    },
  };
  return builder;
}

function notGranted(method) {
  return new PeerError(ERR_CAPABILITY_NOT_GRANTED, `capability not declared for ${method}`);
}

// ---------------------------------------------------------------------------
// Provider bridge (P7)
// ---------------------------------------------------------------------------

function isTerminalEvent(event) {
  return event?.type === "done" || event?.type === "error";
}

/** A zeroed assistant message (valid AssistantMessage JSON) carrying an
 * error, built from the served model's ids. */
function zeroedAssistantMessage(model, errorMessage) {
  const idOf = (key) => (typeof model?.[key] === "string" ? model[key] : "");
  return {
    content: [],
    api: idOf("api"),
    provider: idOf("provider"),
    model: idOf("id"),
    usage: {
      input: 0,
      output: 0,
      cacheRead: 0,
      cacheWrite: 0,
      totalTokens: 0,
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
    },
    stopReason: "error",
    errorMessage,
    timestamp: Date.now(),
  };
}

/** The event sink scoped to one `provider/stream` call: sends
 * `provider/streamEvent` notifications and enforces exactly one terminal
 * event (done/error). */
function makeProviderEvents(peer, streamId, model) {
  let terminalSent = false;
  const events = {
    streamId,
    async send(event) {
      if (isTerminalEvent(event)) {
        if (terminalSent) throw new Error("provider stream already terminated");
        terminalSent = true;
      }
      await peer.notify("provider/streamEvent", { streamId, event });
    },
    textDelta: (contentIndex, delta, partial) =>
      events.send({ type: "textDelta", contentIndex, delta, partial }),
    thinkingDelta: (contentIndex, delta, partial) =>
      events.send({ type: "thinkingDelta", contentIndex, delta, partial }),
    done: (message) =>
      events.send({ type: "done", reason: message?.stopReason ?? "stop", message }),
    error: (errorMessage, message) =>
      events.send({
        type: "error",
        reason: "error",
        error: message ?? zeroedAssistantMessage(model, errorMessage),
      }),
    /** Terminal enforcement: fire an automatic `error` when the handler
     * failed or returned without a terminal event. */
    async enforceTerminal(err) {
      if (err) {
        await events.error(err?.message ?? String(err)).catch(() => {});
        return;
      }
      if (!terminalSent) {
        await events
          .error("provider stream handler returned without a terminal event")
          .catch(() => {});
      }
    },
  };
  return events;
}

/** The plugin → host typed client (ui/exec/session/snapshot/config/…). */
function makeHostClient(peer) {
  const call = (method, params) => peer.call(method, params ?? null);
  const notify = (method, params) => peer.notify(method, params);
  return {
    notify: (message, level) => call("ui/notify", { message, level }),
    select: (title, options) => call("ui/select", { title, options }),
    confirm: (title, message) => call("ui/confirm", { title, message }),
    input: (title, placeholder) => call("ui/input", { title, placeholder }),
    exec: (command, timeoutMs) => call("exec/run", { command, timeoutMs }),
    log: (level, message) => notify("logs/emit", { level, message }),
    warn: (message, context) => notify("warnings/emit", { message, context }),
    session: () => call("session/get"),
    sendUserMessage: (text) => call("session/sendUserMessage", { text }),
    snapshot: () => call("snapshot/get"),
    config: () => call("config/get").then((r) => r.config),
    registerProvider: (provider) => call("host/registerProvider", { provider }),
    providerEvent: (provider, kind, message, detail) =>
      notify("provider/event", { provider, kind, message, detail }),
    widgetUpdate: (update) => notify("widgets/update", update),
  };
}

// ---------------------------------------------------------------------------
// Convenience constructors
// ---------------------------------------------------------------------------

export function textBlock(text) {
  return { type: "text", text };
}

export function textOutput(text) {
  return { content: [textBlock(text)] };
}

export function errorOutput(text) {
  return { content: [textBlock(text)], isError: true };
}

export function allow() {
  return { action: "allow" };
}

export function deny(reason) {
  return { action: "deny", reason };
}

export function rewrite(args) {
  return { action: "rewrite", arguments: args };
}
