"""End-to-end: a real plugin over in-memory streams, driven by a
scripted host peer — the same code path as the stdio carrier."""

import asyncio
import unittest

from tack_plugin import (
    ERR_CAPABILITY_NOT_GRANTED,
    ERR_INVALID_PARAMS,
    ERR_METHOD_NOT_FOUND,
    PROTOCOL_VERSION,
    JsonRpcPeer,
    PeerError,
    Plugin,
    allow,
    deny,
    text_output,
)

ERR_POLICY_DENIED = -32001


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
        "capabilities": {"widgets": True, "uiDialogs": True},
        "config": {"severity": "high"},
    }


def echo_plugin() -> Plugin:
    return Plugin("test-plugin", version="1.0.0").tool(
        {"name": "test.echo", "description": "echo", "parameters": {"type": "object"}},
        lambda params, cx: text_output(f"echo: {params['arguments']}"),
    )


class E2ETest(unittest.IsolatedAsyncioTestCase):
    async def spawn(self, plugin: Plugin, host_handler=None):
        (r1, w1), (r2, w2) = connect()
        host = JsonRpcPeer(r1, w1, host_handler)
        serve = asyncio.ensure_future(plugin.serve(r2, w2))
        return host, serve, w1

    async def test_handshake_advertises_capabilities(self):
        host, serve, _ = await self.spawn(echo_plugin())
        result = await host.call("initialize", init_params())
        self.assertEqual(result["plugin"]["name"], "test-plugin")
        self.assertEqual(result["plugin"]["version"], "1.0.0")
        self.assertEqual([t["name"] for t in result["capabilities"]["tools"]], ["test.echo"])
        self.assertNotIn("hooks", result["capabilities"])
        await host.call("shutdown", None)
        await serve

    async def test_handshake_rejects_incompatible_version(self):
        host, serve, w1 = await self.spawn(echo_plugin())
        with self.assertRaises(PeerError) as raised:
            await host.call("initialize", {**init_params(), "protocolVersion": "4.0.0"})
        self.assertEqual(raised.exception.code, ERR_INVALID_PARAMS)
        self.assertIn("unsupported host protocol", str(raised.exception))
        w1.close()
        await serve

    async def test_tool_execute_roundtrip_and_unknown_tool(self):
        host, serve, _ = await self.spawn(echo_plugin())
        await host.call("initialize", init_params())
        output = await host.call(
            "tools/execute", {"name": "test.echo", "toolCallId": "c-1", "arguments": {"x": 42}}
        )
        self.assertEqual(output["content"][0]["text"], "echo: {'x': 42}")
        with self.assertRaises(PeerError) as raised:
            await host.call(
                "tools/execute", {"name": "nope", "toolCallId": "c-2", "arguments": {}}
            )
        self.assertEqual(raised.exception.code, ERR_INVALID_PARAMS)
        await host.call("shutdown", None)
        await serve

    async def test_before_tool_call_verdicts_and_gating(self):
        def guard(params, cx):
            if params["toolCall"]["toolName"] == "bash":
                return deny("no shell today")
            return allow()

        host, serve, _ = await self.spawn(Plugin("guard").before_tool_call(guard))
        await host.call("initialize", init_params())

        def call(tool_name):
            return host.call(
                "hooks/beforeToolCall",
                {"toolCall": {"toolCallId": "c-1", "toolName": tool_name, "arguments": {}}},
            )

        self.assertEqual(await call("bash"), {"action": "deny", "reason": "no shell today"})
        self.assertEqual(await call("read"), {"action": "allow"})
        await host.call("shutdown", None)
        await serve

        # A plugin without the hook answers ERR_CAPABILITY_NOT_GRANTED.
        host2, serve2, _ = await self.spawn(echo_plugin())
        await host2.call("initialize", init_params())
        with self.assertRaises(PeerError) as raised:
            await host2.call(
                "hooks/beforeToolCall",
                {"toolCall": {"toolCallId": "c-1", "toolName": "bash", "arguments": {}}},
            )
        self.assertEqual(raised.exception.code, ERR_CAPABILITY_NOT_GRANTED)
        await host2.call("shutdown", None)
        await serve2

    async def test_transform_context_null_passthrough(self):
        host, serve, _ = await self.spawn(
            Plugin("ctx").transform_context(lambda params, cx: None)
        )
        await host.call("initialize", init_params())
        result = await host.call("hooks/transformContext", {"messages": []})
        self.assertIsNone(result)
        await host.call("shutdown", None)
        await serve

    async def test_events_and_widget_actions_dispatch(self):
        seen = []

        host, serve, _ = await self.spawn(
            Plugin("observer")
            .events(["turnStart"], lambda params, cx: seen.append(params["event"]))
            .widget({"id": "list", "type": "listPanel", "title": "items"})
            .on_widget_action(
                lambda params, cx: seen.append(f"{params['action']}:{params.get('itemId')}")
            )
        )
        init = await host.call("initialize", init_params())
        self.assertEqual(init["capabilities"]["events"], ["turnStart"])
        self.assertEqual(len(init["capabilities"]["widgets"]), 1)
        await host.notify("events/lifecycle", {"event": "turnStart", "payload": {}})
        await host.notify("widgets/action", {"id": "list", "action": "select", "itemId": "a.rs"})
        for _ in range(50):
            if len(seen) >= 2:
                break
            await asyncio.sleep(0.02)
        self.assertEqual(seen, ["turnStart", "select:a.rs"])
        await host.call("shutdown", None)
        await serve

    async def test_plugin_calls_host_services(self):
        requests = []

        async def host_handler(method, params):
            requests.append(method)
            if method == "ui/select":
                return "b"
            if method == "exec/run":
                raise PeerError(ERR_POLICY_DENIED, "exec requires project trust")
            raise PeerError(ERR_METHOD_NOT_FOUND, f"unknown {method}")

        async def ask(params, cx):
            picked = await cx.host.select("pick", ["a", "b"])
            with self.assertRaises(PeerError) as raised:
                await cx.host.exec("rm -rf /")
            self.assertEqual(raised.exception.code, ERR_POLICY_DENIED)
            return text_output(f"picked={picked} severity={cx.config['severity']}")

        plugin = Plugin("needy").tool(
            {"name": "test.ask", "description": "uses host services", "parameters": {"type": "object"}},
            ask,
        )
        host, serve, _ = await self.spawn(plugin, host_handler)
        await host.call("initialize", init_params())
        output = await host.call(
            "tools/execute", {"name": "test.ask", "toolCallId": "c-1", "arguments": {}}
        )
        self.assertEqual(output["content"][0]["text"], "picked=b severity=high")
        self.assertIn("ui/select", requests)
        self.assertIn("exec/run", requests)
        await host.call("shutdown", None)
        await serve

    async def test_unknown_method_is_method_not_found(self):
        host, serve, _ = await self.spawn(echo_plugin())
        await host.call("initialize", init_params())
        with self.assertRaises(PeerError) as raised:
            await host.call("bogus/method", {})
        self.assertEqual(raised.exception.code, ERR_METHOD_NOT_FOUND)
        await host.call("shutdown", None)
        await serve


if __name__ == "__main__":
    unittest.main()
