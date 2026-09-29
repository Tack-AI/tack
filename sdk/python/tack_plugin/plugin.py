"""Plugin builder + dispatcher + serve loop for tack-RPC v3.

Handlers receive plain dicts (see :mod:`tack_plugin.types` for the
generated TypedDict shapes) and may be sync or async.
"""

from __future__ import annotations

import asyncio
import inspect
import sys
import threading
from typing import Any, Awaitable, Callable, Optional

from .peer import (
    ERR_METHOD_NOT_FOUND,
    ERR_PLUGIN_UNAVAILABLE,
    JsonRpcPeer,
    PeerError,
)
from .provider import ProviderEvents, ProviderStreamCx
from .types import ProviderEventKind

PROTOCOL_VERSION = "3.0.0"
ERR_INVALID_PARAMS = -32602
ERR_CAPABILITY_NOT_GRANTED = -32002


class PluginError(PeerError):
    """Raise from a handler to fail the request with a specific code."""


def _protocol_compatible(peer_version: str) -> bool:
    def parse(version: str) -> tuple[int, int]:
        parts = str(version).split(".")
        major = int(parts[0])
        minor = int(parts[1]) if len(parts) > 1 else 0
        return major, minor

    try:
        peer_major, peer_minor = parse(peer_version)
        our_major, our_minor = parse(PROTOCOL_VERSION)
    except (ValueError, IndexError):
        return False
    return peer_major == our_major and peer_minor <= our_minor


async def _maybe_await(value: Any) -> Any:
    if inspect.isawaitable(value):
        return await value
    return value


class Cx:
    """Handler context: the negotiated host environment + host client."""

    def __init__(self, init: Optional[dict], host: "Host"):
        self._init = init or {}
        self.host = host

    @property
    def mode(self) -> Optional[str]:
        return self._init.get("mode")

    @property
    def trusted(self) -> bool:
        return bool(self._init.get("trusted"))

    @property
    def cwd(self) -> Optional[str]:
        return self._init.get("cwd")

    @property
    def capabilities(self) -> dict:
        return self._init.get("capabilities") or {}

    @property
    def config(self) -> Any:
        return self._init.get("config")


class Host:
    """Plugin → host typed client (ui/exec/session/snapshot/config/…)."""

    def __init__(self, peer: JsonRpcPeer):
        self._peer = peer

    async def notify(self, message: str, level: Optional[str] = None) -> None:
        await self._peer.call("ui/notify", {"message": message, "level": level})

    async def select(self, title: str, options: list) -> Optional[str]:
        return await self._peer.call("ui/select", {"title": title, "options": options})

    async def confirm(self, title: str, message: str) -> bool:
        return bool(await self._peer.call("ui/confirm", {"title": title, "message": message}))

    async def input(self, title: str, placeholder: Optional[str] = None) -> Optional[str]:
        return await self._peer.call("ui/input", {"title": title, "placeholder": placeholder})

    async def exec(self, command: str, timeout_ms: Optional[int] = None) -> dict:
        return await self._peer.call("exec/run", {"command": command, "timeoutMs": timeout_ms})

    async def log(self, level: str, message: str) -> None:
        await self._peer.notify("logs/emit", {"level": level, "message": message})

    async def warn(self, message: str, context: Any = None) -> None:
        await self._peer.notify("warnings/emit", {"message": message, "context": context})

    async def session(self) -> dict:
        return await self._peer.call("session/get", None)

    async def send_user_message(self, text: str) -> None:
        await self._peer.call("session/sendUserMessage", {"text": text})

    async def snapshot(self) -> dict:
        return await self._peer.call("snapshot/get", None)

    async def config(self) -> Any:
        result = await self._peer.call("config/get", None)
        return (result or {}).get("config")

    async def register_provider(self, provider: dict) -> None:
        await self._peer.call("host/registerProvider", {"provider": provider})

    async def provider_event(
        self, provider: str, kind: Any, message: str, detail: Any = None
    ) -> None:
        """``provider/event`` notification: rate-limit/warning/info
        surfaced to the user for a bridge provider. ``kind`` is a
        ProviderEventKind or its string value."""
        kind_value = kind.value if isinstance(kind, ProviderEventKind) else str(kind)
        params: dict[str, Any] = {"provider": provider, "kind": kind_value, "message": message}
        if detail is not None:
            params["detail"] = detail
        await self._peer.notify("provider/event", params)

    async def widget_update(self, update: dict) -> None:
        await self._peer.notify("widgets/update", update)


class Plugin:
    """A tack-RPC v3 plugin. Every capability is optional and
    independent; undeclared capabilities cost nothing."""

    def __init__(self, name: str, version: Optional[str] = None, description: Optional[str] = None):
        self._name = name
        self._version = version
        self._description = description
        self._tools: dict[str, tuple[dict, Callable]] = {}
        self._commands: dict[str, tuple[dict, Callable]] = {}
        self._hooks: dict[str, Callable] = {}
        self._events: list[str] = []
        self._event_handler: Optional[Callable] = None
        self._widgets: list[dict] = []
        self._widget_action_handler: Optional[Callable] = None
        self._autocomplete: dict[str, tuple[dict, Callable]] = {}
        self._config_schema: Optional[dict] = None
        self._metrics: Optional[dict] = None
        self._provider_stream_handler: Optional[Callable] = None
        self._on_ready: Optional[Callable] = None
        # streamId -> cancellation event for in-flight provider/stream
        # handlers (provider/streamCancel sets it; entries drop on
        # handler completion).
        self._provider_streams: dict[str, asyncio.Event] = {}
        # Strong refs for fire-and-forget tasks (on_ready, stream
        # supervisors) so the loop never garbage-collects them.
        self._background: set[asyncio.Task] = set()

    # -- registration (chainable) -------------------------------------

    def tool(self, spec: dict, handler: Callable) -> "Plugin":
        self._tools[spec["name"]] = (spec, handler)
        return self

    def command(self, name: str, description: Optional[str], handler: Callable) -> "Plugin":
        spec: dict[str, Any] = {"name": name}
        if description is not None:
            spec["description"] = description
        self._commands[name] = (spec, handler)
        return self

    def before_tool_call(self, handler: Callable) -> "Plugin":
        self._hooks["beforeToolCall"] = handler
        return self

    def after_tool_call(self, handler: Callable) -> "Plugin":
        self._hooks["afterToolCall"] = handler
        return self

    def transform_context(self, handler: Callable) -> "Plugin":
        self._hooks["transformContext"] = handler
        return self

    def approval_review(self, handler: Callable) -> "Plugin":
        self._hooks["approvalReview"] = handler
        return self

    def events(self, names: list, handler: Callable) -> "Plugin":
        self._events = list(names)
        self._event_handler = handler
        return self

    def widget(self, spec: dict) -> "Plugin":
        self._widgets.append(spec)
        return self

    def on_widget_action(self, handler: Callable) -> "Plugin":
        self._widget_action_handler = handler
        return self

    def autocomplete(self, spec: dict, handler: Callable) -> "Plugin":
        self._autocomplete[spec["id"]] = (spec, handler)
        return self

    def config_schema(self, schema: dict) -> "Plugin":
        self._config_schema = schema
        return self

    def metrics(self, declaration: dict) -> "Plugin":
        self._metrics = declaration
        return self

    def provider_stream(self, handler: Callable) -> "Plugin":
        """Serve inference for registered providers (the P7 provider
        bridge): declares the ``provider.stream`` capability; the host
        calls ``provider/stream`` for every turn on the models of
        providers this plugin registered with ``bridge: True``. The
        handler receives ``(params, events, stream_cx)`` and may be
        sync or async; events ride :class:`ProviderEvents` and
        cancellation surfaces on :class:`ProviderStreamCx`."""
        self._provider_stream_handler = handler
        return self

    def on_ready(self, handler: Callable) -> "Plugin":
        """Run once after the initialize handshake is answered — the
        registration entry point for provider plugins (call
        ``cx.host.register_provider(...)`` here) and for any plugin
        that pushes state at startup. Host services gate registrations
        on the completed handshake, so registering immediately is
        safe. The handler receives ``(cx)`` and may be sync or async."""
        self._on_ready = handler
        return self

    # -- serving --------------------------------------------------------

    def run(self) -> None:
        """Serve over stdio (blocks; returns on shutdown/EOF)."""
        asyncio.run(self.serve())

    async def serve(self, reader: Any = None, writer: Any = None) -> None:
        """Serve over a custom transport (default: stdio)."""
        if reader is None or writer is None:
            reader, writer = _stdio_streams()
        init: dict[str, Any] = {}
        shutdown = asyncio.Event()
        host: list[Optional[Host]] = [None]

        def cx() -> Cx:
            return Cx(init.get("params"), host[0])

        def on_initialize(params: dict) -> dict:
            if not _protocol_compatible(params.get("protocolVersion", "")):
                raise PluginError(
                    ERR_INVALID_PARAMS,
                    f"unsupported host protocol {params.get('protocolVersion')} "
                    f"(this plugin speaks {PROTOCOL_VERSION})",
                )
            init["params"] = params
            capabilities: dict[str, Any] = {}
            if self._tools:
                capabilities["tools"] = [spec for spec, _ in self._tools.values()]
            if self._commands:
                capabilities["commands"] = [spec for spec, _ in self._commands.values()]
            hooks = {
                "beforeToolCall": True if "beforeToolCall" in self._hooks else None,
                "transformContext": True if "transformContext" in self._hooks else None,
                "afterToolCall": True if "afterToolCall" in self._hooks else None,
                "approvalReview": True if "approvalReview" in self._hooks else None,
            }
            hooks = {key: value for key, value in hooks.items() if value}
            if hooks:
                capabilities["hooks"] = hooks
            if self._event_handler is not None:
                capabilities["events"] = self._events
            if self._widgets:
                capabilities["widgets"] = self._widgets
            if self._autocomplete:
                capabilities["autocompleteProviders"] = [
                    spec for spec, _ in self._autocomplete.values()
                ]
            if self._config_schema is not None:
                capabilities["config"] = {"schema": self._config_schema}
            if self._metrics is not None:
                capabilities["metrics"] = self._metrics
            if self._provider_stream_handler is not None:
                capabilities["provider"] = {"stream": True}
            plugin_info: dict[str, Any] = {"name": self._name}
            if self._version is not None:
                plugin_info["version"] = self._version
            if self._description is not None:
                plugin_info["description"] = self._description
            # The startup hook (provider plugins register their
            # providers here). Spawned: on_ready must not delay the
            # handshake answer.
            if self._on_ready is not None:
                self._spawn(self._run_ready(cx()))
            return {
                "protocolVersion": PROTOCOL_VERSION,
                "plugin": plugin_info,
                "capabilities": capabilities,
            }

        async def handle_request(method: str, params: Any) -> Any:
            params = params or {}
            if method == "initialize":
                return on_initialize(params)
            if method == "shutdown":
                shutdown.set()
                return None
            if method == "tools/execute":
                entry = self._tools.get(params.get("name"))
                if entry is None:
                    raise PluginError(ERR_INVALID_PARAMS, f"unknown tool {params.get('name')!r}")
                return await _maybe_await(entry[1](params, cx()))
            if method == "commands/invoke":
                entry = self._commands.get(params.get("name"))
                if entry is None:
                    raise PluginError(ERR_INVALID_PARAMS, f"unknown command {params.get('name')!r}")
                return await _maybe_await(entry[1](params, cx()))
            if method == "hooks/beforeToolCall":
                return await self._call_hook("beforeToolCall", method, params, cx)
            if method == "hooks/afterToolCall":
                return await self._call_hook("afterToolCall", method, params, cx)
            if method == "hooks/transformContext":
                return await self._call_hook("transformContext", method, params, cx)
            if method == "approval/review":
                return await self._call_hook("approvalReview", method, params, cx)
            if method == "autocomplete/provide":
                entry = self._autocomplete.get(params.get("providerId"))
                if entry is None:
                    raise PluginError(
                        ERR_INVALID_PARAMS,
                        f"unknown autocomplete provider {params.get('providerId')!r}",
                    )
                return await _maybe_await(entry[1](params, cx()))
            if method == "provider/stream":
                if self._provider_stream_handler is None:
                    raise PluginError(
                        ERR_CAPABILITY_NOT_GRANTED, f"capability not declared for {method}"
                    )
                return self._open_provider_stream(params, cx(), host[0])
            raise PluginError(ERR_METHOD_NOT_FOUND, f"unknown method {method}")

        async def handle_notification(method: str, params: Any) -> None:
            params = params or {}
            if method == "events/lifecycle" and self._event_handler is not None:
                await _maybe_await(self._event_handler(params, cx()))
            elif method == "widgets/action" and self._widget_action_handler is not None:
                await _maybe_await(self._widget_action_handler(params, cx()))
            elif method == "provider/streamCancel":
                cancel = self._provider_streams.get(params.get("streamId"))
                if cancel is not None:
                    cancel.set()

        peer = JsonRpcPeer(reader, writer, handle_request, handle_notification)
        host[0] = Host(peer)
        await asyncio.wait(
            [asyncio.ensure_future(shutdown.wait()), asyncio.ensure_future(peer.wait_dead())],
            return_when=asyncio.FIRST_COMPLETED,
        )

    async def _call_hook(self, name: str, method: str, params: Any, cx: Callable[[], Cx]) -> Any:
        handler = self._hooks.get(name)
        if handler is None:
            raise PluginError(ERR_CAPABILITY_NOT_GRANTED, f"capability not declared for {method}")
        return await _maybe_await(handler(params, cx()))

    # -- provider bridge (P7) -------------------------------------------

    def _spawn(self, coro: Awaitable[Any]) -> None:
        task = asyncio.ensure_future(coro)
        self._background.add(task)
        task.add_done_callback(self._background.discard)

    async def _run_ready(self, ready_cx: Cx) -> None:
        try:
            await _maybe_await(self._on_ready(ready_cx))
        except Exception:  # noqa: BLE001 — startup-hook errors are non-fatal
            pass

    def _open_provider_stream(self, params: dict, stream_cx_base: Cx, host: Optional[Host]) -> None:
        stream_id = params.get("streamId")
        if not isinstance(stream_id, str) or not stream_id:
            raise PluginError(ERR_INVALID_PARAMS, "provider/stream requires a streamId")
        if host is None:
            raise PluginError(ERR_PLUGIN_UNAVAILABLE, "plugin is not serving")
        cancel = asyncio.Event()
        self._provider_streams[stream_id] = cancel
        events = ProviderEvents(host._peer, stream_id, params.get("model"))
        stream_cx = ProviderStreamCx(stream_cx_base, stream_id, cancel)
        # The ack is fast: validation is done; the stream rides
        # provider/streamEvent notifications from here on.
        self._spawn(self._supervise_provider_stream(stream_id, params, events, stream_cx))
        return None

    async def _supervise_provider_stream(
        self,
        stream_id: str,
        params: dict,
        events: ProviderEvents,
        stream_cx: ProviderStreamCx,
    ) -> None:
        """Await the handler so an exception still produces the
        automatic terminal error event; then drop the cancel entry."""
        try:
            failure: Optional[str]
            try:
                await _maybe_await(self._provider_stream_handler(params, events, stream_cx))
            except Exception as err:  # noqa: BLE001 — handler failure becomes the terminal error
                failure = str(err) or type(err).__name__
            else:
                failure = (
                    None
                    if events.terminal_sent
                    else "provider stream handler returned without a terminal event"
                )
            if failure is not None:
                try:
                    await events.error(failure)
                except PeerError:
                    pass
        finally:
            self._provider_streams.pop(stream_id, None)


# ---------------------------------------------------------------------------
# stdio transport
# ---------------------------------------------------------------------------


class _StdoutWriter:
    """Blocking stdout writer (payloads are small; writes are serialized
    by the peer's write lock)."""

    def __init__(self):
        self._lock = threading.Lock()

    def write(self, data: bytes) -> None:
        with self._lock:
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()


def _stdio_streams() -> tuple[asyncio.StreamReader, _StdoutWriter]:
    """A StreamReader fed from stdin on a daemon thread + the stdout
    writer. (Cross-platform: no connect_read_pipe portability traps.)"""
    loop = asyncio.get_running_loop()
    reader: asyncio.StreamReader = asyncio.StreamReader()

    def pump() -> None:
        while True:
            line = sys.stdin.buffer.readline()
            if not line:
                loop.call_soon_threadsafe(reader.feed_eof)
                return
            loop.call_soon_threadsafe(reader.feed_data, line)

    threading.Thread(target=pump, daemon=True).start()
    return reader, _StdoutWriter()


# ---------------------------------------------------------------------------
# Convenience constructors
# ---------------------------------------------------------------------------


def text_block(text: str) -> dict:
    return {"type": "text", "text": text}


def text_output(text: str) -> dict:
    return {"content": [text_block(text)]}


def error_output(text: str) -> dict:
    return {"content": [text_block(text)], "isError": True}


def allow() -> dict:
    return {"action": "allow"}


def deny(reason: str) -> dict:
    return {"action": "deny", "reason": reason}


def rewrite(arguments: Any) -> dict:
    return {"action": "rewrite", "arguments": arguments}
