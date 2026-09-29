"""The provider bridge surface (P7): serve inference for registered
providers through ``provider/stream``.

Register :meth:`tack_plugin.Plugin.provider_stream` to declare the
``provider.stream`` capability, then call
:meth:`tack_plugin.Host.register_provider` with ``bridge: True``
(typically from :meth:`tack_plugin.Plugin.on_ready`) — the host starts
sending ``provider/stream`` requests for the provider's models and the
turn's events flow back through :class:`ProviderEvents`. The SDK owns
the plumbing: streamId scoping, the ack/cancel wiring, and terminal
enforcement (exactly one terminal event, with an automatic ``error``
when the handler raises or returns without one).
"""

from __future__ import annotations

import asyncio
import time
from typing import Any, Optional

from .peer import ERR_INTERNAL, JsonRpcPeer, PeerError

_TERMINAL_TYPES = ("done", "error")


def _zeroed_error_message(model: Any, error_message: str) -> dict:
    """A zeroed assistant message (valid AssistantMessage JSON) carrying
    an error, built from the served model's ids."""
    model = model if isinstance(model, dict) else {}

    def id_of(key: str) -> str:
        value = model.get(key)
        return value if isinstance(value, str) else ""

    return {
        "content": [],
        "api": id_of("api"),
        "provider": id_of("provider"),
        "model": id_of("id"),
        "usage": {
            "input": 0,
            "output": 0,
            "cacheRead": 0,
            "cacheWrite": 0,
            "totalTokens": 0,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
        },
        "stopReason": "error",
        "errorMessage": error_message,
        "timestamp": int(time.time() * 1000),
    }


class ProviderEvents:
    """Event sink scoped to one ``provider/stream`` call: sends
    ``provider/streamEvent`` notifications and enforces exactly one
    terminal event."""

    def __init__(self, peer: JsonRpcPeer, stream_id: str, model: Any):
        self._peer = peer
        self._stream_id = stream_id
        self._model = model  # the served model (for synthesized error messages)
        self._terminal_sent = False

    @property
    def stream_id(self) -> str:
        """The stream these events belong to."""
        return self._stream_id

    @property
    def terminal_sent(self) -> bool:
        return self._terminal_sent

    async def send(self, event: dict) -> None:
        """Send one AssistantMessageEvent-shaped event. Terminal events
        (``done``/``error``) may be sent exactly once; a second one
        raises PluginError."""
        if isinstance(event, dict) and event.get("type") in _TERMINAL_TYPES:
            if self._terminal_sent:
                from .plugin import PluginError  # late import: plugin imports this module

                raise PluginError(ERR_INTERNAL, "provider stream already terminated")
            self._terminal_sent = True
        await self._notify(event)

    async def _notify(self, event: dict) -> None:
        await self._peer.notify(
            "provider/streamEvent", {"streamId": self._stream_id, "event": event}
        )

    async def text_delta(self, content_index: int, delta: str, partial: Any) -> None:
        """``textDelta`` convenience (``partial`` is the accumulated message)."""
        await self.send(
            {
                "type": "textDelta",
                "contentIndex": content_index,
                "delta": delta,
                "partial": partial,
            }
        )

    async def thinking_delta(self, content_index: int, delta: str, partial: Any) -> None:
        """``thinkingDelta`` convenience (``partial`` is the accumulated message)."""
        await self.send(
            {
                "type": "thinkingDelta",
                "contentIndex": content_index,
                "delta": delta,
                "partial": partial,
            }
        )

    async def done(self, message: dict) -> None:
        """Terminal ``done`` event; ``reason`` defaults to the message's
        ``stopReason`` (or ``stop``)."""
        reason = message.get("stopReason") if isinstance(message, dict) else None
        if not isinstance(reason, str):
            reason = "stop"
        await self.send({"type": "done", "reason": reason, "message": message})

    async def error(self, error_message: str, message: Optional[dict] = None) -> None:
        """Terminal ``error`` event. ``message`` defaults to a zeroed
        assistant message built from the served model carrying
        ``error_message``."""
        if message is None:
            message = _zeroed_error_message(self._model, error_message)
        await self.send({"type": "error", "reason": "error", "error": message})


class ProviderStreamCx:
    """Stream-scoped context: the plugin's host environment plus the
    stream's cancellation signal (``provider/streamCancel``)."""

    def __init__(self, cx: Any, stream_id: str, cancel: asyncio.Event):
        self.cx = cx
        self.stream_id = stream_id
        self._cancel = cancel

    @property
    def host(self) -> Any:
        """The host client (shortcut for ``cx.host``)."""
        return self.cx.host

    def is_cancelled(self) -> bool:
        return self._cancel.is_set()

    async def cancelled(self) -> None:
        """Resolve when the host cancels the stream
        (``provider/streamCancel``). Ignoring it is legal — the host
        synthesizes a terminal event after its grace period — but
        discouraged."""
        await self._cancel.wait()


__all__ = ["ProviderEvents", "ProviderStreamCx"]
