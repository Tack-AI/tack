"""JsonRpcPeer: transport-agnostic JSON-RPC 2.0 peer for tack-RPC v3.

`reader` is any object with an async ``readline()`` returning ``b""`` on
EOF (e.g. :class:`asyncio.StreamReader`); `writer` any object with
``write(bytes)`` and an async ``drain()``. Both sides may issue requests
concurrently; ids are per-sender.
"""

from __future__ import annotations

import asyncio
import json
from typing import Any, Awaitable, Callable, Optional

JSONRPC_VERSION = "2.0"
CANCEL_METHOD = "$/cancelRequest"

ERR_PARSE = -32700
ERR_METHOD_NOT_FOUND = -32601
ERR_INTERNAL = -32603
ERR_PLUGIN_UNAVAILABLE = -32003
ERR_REQUEST_TIMEOUT = -32004

MAX_LINE_BYTES = 16 * 1024 * 1024
DEFAULT_REQUEST_TIMEOUT = 30.0


class PeerError(Exception):
    """A failed outgoing call; ``code`` is the JSON-RPC error code
    (domain codes like ERR_POLICY_DENIED survive the trip)."""

    def __init__(self, code: int, message: str, data: Any = None):
        super().__init__(message)
        self.code = code
        self.data = data


RequestHandler = Callable[[str, Any], Awaitable[Any]]
NotificationHandler = Callable[[str, Any], Awaitable[None]]

# Pending-map outcome: futures carry (ok, value_or_error) tuples so a
# dead peer can fail every waiter without "exception never retrieved"
# warnings.
_Outcome = tuple[bool, Any]


async def _default_request_handler(method: str, _params: Any) -> Any:
    raise PeerError(ERR_METHOD_NOT_FOUND, f"unknown method {method}")


async def _default_notification_handler(_method: str, _params: Any) -> None:
    return None


class JsonRpcPeer:
    """One live JSON-RPC 2.0 connection (see module docstring)."""

    def __init__(
        self,
        reader: Any,
        writer: Any,
        handle_request: Optional[RequestHandler] = None,
        handle_notification: Optional[NotificationHandler] = None,
    ):
        self._reader = reader
        self._writer = writer
        self._handle_request = handle_request or _default_request_handler
        self._handle_notification = handle_notification or _default_notification_handler
        self._pending: dict[int, asyncio.Future] = {}
        self._inflight: dict[int, asyncio.Task] = {}
        self._next_id = 1
        self._alive = True
        self._write_lock = asyncio.Lock()
        self._pump = asyncio.ensure_future(self._read_pump())

    @property
    def alive(self) -> bool:
        return self._alive

    async def wait_dead(self) -> None:
        try:
            await self._pump
        except asyncio.CancelledError:
            pass

    async def call(self, method: str, params: Any, timeout: float = DEFAULT_REQUEST_TIMEOUT) -> Any:
        if not self._alive:
            raise PeerError(ERR_PLUGIN_UNAVAILABLE, "peer is unavailable")
        request_id = self._next_id
        self._next_id += 1
        future: asyncio.Future = asyncio.get_running_loop().create_future()
        self._pending[request_id] = future
        try:
            await self._write_line(
                json.dumps(
                    {"jsonrpc": JSONRPC_VERSION, "id": request_id, "method": method, "params": params}
                )
            )
            ok, value = await asyncio.wait_for(future, timeout)
        except asyncio.TimeoutError:
            await self._send_cancel(request_id)
            raise PeerError(ERR_REQUEST_TIMEOUT, f"request timed out: {method}") from None
        finally:
            self._pending.pop(request_id, None)
        if not ok:
            raise value
        return value

    async def notify(self, method: str, params: Any) -> None:
        if not self._alive:
            raise PeerError(ERR_PLUGIN_UNAVAILABLE, "peer is unavailable")
        await self._write_line(
            json.dumps({"jsonrpc": JSONRPC_VERSION, "method": method, "params": params})
        )

    async def _send_cancel(self, request_id: int) -> None:
        try:
            await self.notify(CANCEL_METHOD, {"id": request_id})
        except PeerError:
            pass

    async def _write_line(self, line: str) -> None:
        async with self._write_lock:
            try:
                self._writer.write(line.encode("utf-8") + b"\n")
                drain = getattr(self._writer, "drain", None)
                if drain is not None:
                    result = drain()
                    if asyncio.isfuture(result) or asyncio.iscoroutine(result):
                        await result
            except Exception as err:  # noqa: BLE001 — transport failure
                raise PeerError(ERR_PLUGIN_UNAVAILABLE, f"write failed: {err}") from err

    async def _respond(self, request_id: Any, result: Any = None, error: Optional[PeerError] = None) -> None:
        message: dict[str, Any] = {"jsonrpc": JSONRPC_VERSION, "id": request_id}
        if error is not None:
            message["error"] = {"code": error.code, "message": str(error), "data": error.data}
        else:
            message["result"] = result
        await self._write_line(json.dumps(message))

    def _dispatch(self, message: dict) -> None:
        method = message.get("method")
        request_id = message.get("id")
        params = message.get("params")
        if method is not None and request_id is not None:
            # Incoming request: answer in a tracked task (cancellable).
            task = asyncio.ensure_future(self._run_handler(method, request_id, params))
            self._inflight[request_id] = task
        elif method is not None:
            if method == CANCEL_METHOD:
                cancel_id = (params or {}).get("id")
                task = self._inflight.pop(cancel_id, None)
                if task is not None:
                    task.cancel()
                return
            asyncio.ensure_future(self._run_notification(method, params))
        elif request_id is not None:
            future = self._pending.pop(request_id, None)
            if future is None or future.done():
                return
            error = message.get("error")
            if error is not None:
                future.set_result(
                    (False, PeerError(error.get("code", ERR_INTERNAL), error.get("message", ""), error.get("data")))
                )
            else:
                future.set_result((True, message.get("result")))

    async def _run_handler(self, method: str, request_id: Any, params: Any) -> None:
        try:
            result = await self._handle_request(method, params)
            await self._respond(request_id, result=result)
        except asyncio.CancelledError:
            return  # cancelled by the peer: no response
        except PeerError as error:
            await self._respond(request_id, error=error)
        except Exception as err:  # noqa: BLE001 — handler bug becomes ERR_INTERNAL
            await self._respond(request_id, error=PeerError(ERR_INTERNAL, str(err)))
        finally:
            self._inflight.pop(request_id, None)

    async def _run_notification(self, method: str, params: Any) -> None:
        try:
            await self._handle_notification(method, params)
        except Exception:  # noqa: BLE001 — notifications are fire-and-forget
            pass

    async def _read_pump(self) -> None:
        try:
            while True:
                line = await self._reader.readline()
                if not line:
                    break
                if len(line) > MAX_LINE_BYTES:
                    break  # hostile/broken peer: declare dead
                text = line.decode("utf-8", errors="replace").strip()
                if not text:
                    continue
                try:
                    message = json.loads(text)
                except json.JSONDecodeError:
                    await self._respond(None, error=PeerError(ERR_PARSE, "parse error"))
                    continue
                if isinstance(message, dict):
                    self._dispatch(message)
        except Exception:  # noqa: BLE001 — any read failure is terminal
            pass
        self._mark_dead()

    def _mark_dead(self) -> None:
        if not self._alive:
            return
        self._alive = False
        for future in self._pending.values():
            if not future.done():
                future.set_result(
                    (False, PeerError(ERR_PLUGIN_UNAVAILABLE, "peer is unavailable"))
                )
        self._pending.clear()
        for task in self._inflight.values():
            task.cancel()
        self._inflight.clear()
