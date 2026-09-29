"""Provider bridge e2e tests (P7): a plugin serving inference over an
in-memory duplex, driven by a scripted host peer — the same code path
as the stdio carrier. Mirrors crates/tack-ext-sdk/tests/provider_e2e.rs."""

import asyncio
import unittest

from tack_plugin import (
    ERR_CAPABILITY_NOT_GRANTED,
    ERR_METHOD_NOT_FOUND,
    PROTOCOL_VERSION,
    JsonRpcPeer,
    PeerError,
    Plugin,
)


class MemoryWriter:
    """Writes feed the connected peer's reader."""

    def __init__(self, reader: asyncio.StreamReader):
        self._reader = reader

    def write(self, data: bytes) -> None:
        self._reader.feed_data(data)

    async def drain(self) -> None:
        return None

    def close(self) -> None:
        self._reader.feed_eof()


def connect():
    """A connected in-memory pair: (reader1, writer1), (reader2, writer2)
    where writer1 feeds reader2 and vice versa."""
    r1: asyncio.StreamReader = asyncio.StreamReader()
    r2: asyncio.StreamReader = asyncio.StreamReader()
    return (r1, MemoryWriter(r2)), (r2, MemoryWriter(r1))


def init_params():
    return {
        "protocolVersion": PROTOCOL_VERSION,
        "host": {"name": "tack", "version": "test"},
        "mode": "tui",
        "cwd": "/tmp",
        "trusted": True,
        "capabilities": {"providerRegistration": True},
        "config": None,
    }


def stream_params(text: str) -> dict:
    return {
        "streamId": "ps-test-1",
        "model": {"id": "fake-1", "provider": "demo-provider", "api": "ext-provider-bridge"},
        "context": {"messages": [{"role": "user", "content": text, "timestamp": 0}]},
        "options": {"maxTokens": 1024},
    }


def partial(model: dict) -> dict:
    """A pending assistant message skeleton for scripted events."""
    return {
        "content": [],
        "api": model.get("api", ""),
        "provider": model.get("provider", ""),
        "model": model.get("id", ""),
        "usage": {
            "input": 0,
            "output": 0,
            "cacheRead": 0,
            "cacheWrite": 0,
            "totalTokens": 0,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
        },
        "stopReason": "pending",
        "timestamp": 0,
    }


class HostStub:
    """Captures provider registrations and stream events."""

    def __init__(self):
        self.registrations = []
        self.stream_events = []  # list of (stream_id, event)

    async def handle_request(self, method, params):
        if method == "host/registerProvider":
            self.registrations.append((params or {}).get("provider"))
            return None
        raise PeerError(ERR_METHOD_NOT_FOUND, f"stub: unknown {method}")

    async def handle_notification(self, method, params):
        if method == "provider/streamEvent":
            self.stream_events.append(((params or {}).get("streamId"), (params or {}).get("event")))


async def wait_for_events(stub: HostStub, count: int):
    for _ in range(100):
        if len(stub.stream_events) >= count:
            return list(stub.stream_events)
        await asyncio.sleep(0.02)
    raise AssertionError(f"timed out waiting for {count} stream events")


def echo_plugin() -> Plugin:
    async def handler(params, events, _cx):
        await events.send({"type": "start", "partial": partial(params["model"])})
        await events.text_delta(0, "hello", partial(params["model"]))
        message = partial(params["model"])
        message["content"] = [{"type": "text", "text": "hello"}]
        message["stopReason"] = "stop"
        await events.done(message)

    return Plugin("provider-plugin").provider_stream(handler)


class ProviderE2ETest(unittest.IsolatedAsyncioTestCase):
    async def spawn(self, plugin: Plugin):
        (r1, w1), (r2, w2) = connect()
        stub = HostStub()
        host = JsonRpcPeer(r1, w1, stub.handle_request, stub.handle_notification)
        serve = asyncio.ensure_future(plugin.serve(r2, w2))
        init = await host.call("initialize", init_params())
        return host, serve, stub, init

    async def test_capability_advertised_and_stream_events_flow(self):
        host, serve, stub, init = await self.spawn(echo_plugin())
        # The handshake advertised provider.stream.
        self.assertEqual(init["capabilities"]["provider"], {"stream": True})
        ack = await host.call("provider/stream", stream_params("hi"))
        self.assertIsNone(ack)  # fast ack
        captured = await wait_for_events(stub, 3)
        self.assertTrue(all(stream_id == "ps-test-1" for stream_id, _ in captured))
        self.assertEqual(captured[0][1]["type"], "start")
        self.assertEqual(captured[1][1]["type"], "textDelta")
        self.assertEqual(captured[1][1]["delta"], "hello")
        self.assertEqual(captured[2][1]["type"], "done")
        self.assertEqual(captured[2][1]["message"]["stopReason"], "stop")
        await host.call("shutdown", None)
        await serve

    async def test_on_ready_registers_the_provider(self):
        async def noop_stream(_params, _events, _cx):
            return None

        async def on_ready(cx):
            await cx.host.register_provider(
                {"id": "demo-provider", "bridge": True, "models": [{"id": "fake-1"}]}
            )

        plugin = Plugin("provider-plugin").provider_stream(noop_stream).on_ready(on_ready)
        host, serve, stub, _ = await self.spawn(plugin)
        for _ in range(100):
            if stub.registrations:
                break
            await asyncio.sleep(0.02)
        self.assertEqual(len(stub.registrations), 1)
        self.assertEqual(stub.registrations[0]["id"], "demo-provider")
        self.assertEqual(stub.registrations[0]["bridge"], True)
        await host.call("shutdown", None)
        await serve

    async def test_missing_terminal_fires_the_automatic_error(self):
        async def handler(params, events, _cx):
            await events.send({"type": "start", "partial": partial(params["model"])})
            # no terminal: SDK enforcement fires

        host, serve, stub, _ = await self.spawn(
            Plugin("provider-plugin").provider_stream(handler)
        )
        await host.call("provider/stream", stream_params("hi"))
        captured = await wait_for_events(stub, 2)
        self.assertEqual(captured[0][1]["type"], "start")
        self.assertEqual(captured[1][1]["type"], "error")
        self.assertIn("without a terminal event", captured[1][1]["error"]["errorMessage"])
        # The synthesized error message is a valid assistant message shape.
        self.assertEqual(captured[1][1]["error"]["provider"], "demo-provider")
        await host.call("shutdown", None)
        await serve

    async def test_handler_error_becomes_the_terminal_error_event(self):
        async def handler(_params, _events, _cx):
            raise RuntimeError("backend exploded")

        host, serve, stub, _ = await self.spawn(
            Plugin("provider-plugin").provider_stream(handler)
        )
        await host.call("provider/stream", stream_params("hi"))
        captured = await wait_for_events(stub, 1)
        self.assertEqual(captured[0][1]["type"], "error")
        self.assertEqual(captured[0][1]["error"]["errorMessage"], "backend exploded")
        await host.call("shutdown", None)
        await serve

    async def test_second_terminal_event_is_rejected(self):
        rejected = []

        async def handler(params, events, _cx):
            message = partial(params["model"])
            await events.done(message)
            try:
                await events.done(dict(message))
            except PeerError as err:
                rejected.append(err)

        host, serve, stub, _ = await self.spawn(
            Plugin("provider-plugin").provider_stream(handler)
        )
        await host.call("provider/stream", stream_params("hi"))
        captured = await wait_for_events(stub, 1)
        await asyncio.sleep(0.1)
        self.assertEqual(len(stub.stream_events), 1, "exactly one terminal crossed the wire")
        self.assertEqual(captured[0][1]["type"], "done")
        self.assertEqual(len(rejected), 1, "a second terminal must be rejected")
        self.assertIn("already terminated", str(rejected[0]))
        await host.call("shutdown", None)
        await serve

    async def test_stream_cancel_reaches_the_handler(self):
        async def handler(params, events, cx):
            await events.send({"type": "start", "partial": partial(params["model"])})
            await cx.cancelled()
            self.assertTrue(cx.is_cancelled())
            error = partial(params["model"])
            error["stopReason"] = "aborted"
            error["errorMessage"] = "demo aborted"
            await events.send({"type": "error", "reason": "aborted", "error": error})

        host, serve, stub, _ = await self.spawn(
            Plugin("provider-plugin").provider_stream(handler)
        )
        await host.call("provider/stream", stream_params("hi"))
        await wait_for_events(stub, 1)  # start landed
        await host.notify("provider/streamCancel", {"streamId": "ps-test-1"})
        captured = await wait_for_events(stub, 2)
        self.assertEqual(captured[1][1]["type"], "error")
        self.assertEqual(captured[1][1]["reason"], "aborted")
        await host.call("shutdown", None)
        await serve

    async def test_undeclared_provider_stream_is_capability_not_granted(self):
        host, serve, _, _ = await self.spawn(Plugin("plain-plugin"))
        with self.assertRaises(PeerError) as raised:
            await host.call("provider/stream", stream_params("hi"))
        self.assertEqual(raised.exception.code, ERR_CAPABILITY_NOT_GRANTED)
        await host.call("shutdown", None)
        await serve

    async def test_options_and_context_arrive_verbatim(self):
        seen = []

        async def handler(params, events, _cx):
            seen.append(params)
            await events.error("seen")

        host, serve, stub, _ = await self.spawn(
            Plugin("provider-plugin").provider_stream(handler)
        )
        params = stream_params("check")
        await host.call("provider/stream", params)
        captured = await wait_for_events(stub, 1)
        self.assertEqual(seen[0]["options"]["maxTokens"], 1024)
        self.assertEqual(seen[0]["context"]["messages"][0]["content"], "check")
        self.assertEqual(seen[0]["streamId"], "ps-test-1")
        self.assertEqual(seen[0]["model"], params["model"])
        self.assertEqual(captured[0][1]["error"]["errorMessage"], "seen")
        await host.call("shutdown", None)
        await serve


if __name__ == "__main__":
    unittest.main()
