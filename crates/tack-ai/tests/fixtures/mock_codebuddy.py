#!/usr/bin/env python3
"""Mock codebuddy CLI for tack-ai codebuddy provider tests.

Implements the stream-json protocol subset the provider uses:
- control_request initialize -> control_response (with a models array);
  when the provider declares sdkMcpServers, the mock runs the MCP
  handshake (initialize/notifications initialized/tools/list) on the first
  user message — exactly like the real CLI's SDK-MCP client
- system init line at startup
- user message "use-echo-tool" -> assistant(tool_use mcp__tack__echo);
  "stream-tool" -> the same via stream_event partials + message_stop
- both tool paths then issue the tools/call as an mcp_message
  control_request and PARK waiting for the provider's control_response
  (the real CLI dispatches the MCP call right after the tool boundary),
  emit the stale assistant echo + user tool_result echo, then continue
  via stream events and a final result
- any other user message -> assistant(text echo) + result
"""
import json
import os
import queue
import re
import sys
import threading
import time

SESSION = "mock-session-1"
MCP_SERVER = "tack"

# Tests assert on the exact spawn flags the provider passes; logged per
# --model value so concurrent test sessions don't overwrite each other.
# requests-<model>.log records SPAWN (process start) and every
# control_request subtype, so tests can tell hot-switches (set_model, no
# respawn) from rebuilds (a second SPAWN).
argv_log_dir = os.environ.get("CODEBUDDY_MOCK_ARGV_LOG_DIR")
req_log = None
if argv_log_dir:
    model = "unknown"
    if "--model" in sys.argv:
        model = sys.argv[sys.argv.index("--model") + 1]
    safe = "".join(c if c.isalnum() or c in "-_" else "_" for c in model)
    with open(os.path.join(argv_log_dir, f"argv-{safe}.json"), "w") as fh:
        fh.write(json.dumps(sys.argv[1:]))
    req_log = os.path.join(argv_log_dir, f"requests-{safe}.log")
    with open(req_log, "a") as fh:
        fh.write(f"SPAWN {model}\n")
    # Distinct session id per model: tests share the JSONL session store
    # (same cwd hash), so a fixed id would race across parallel tests.
    SESSION = f"mock-session-{safe}"

# --resume <sid>: the JSONL rebuild path. Verify the session file the
# provider wrote exists (mirrors codebuddy_jsonl::session_jsonl_path).
if "--resume" in sys.argv:
    sid = sys.argv[sys.argv.index("--resume") + 1]
    config_dir = os.environ.get(
        "CODEBUDDY_CONFIG_DIR", os.path.expanduser("~/.codebuddy"))
    cwd = os.getcwd()
    norm = cwd.rstrip("/") or cwd
    path_hash = re.sub(r"[/\\\\:]", "-", norm)
    path_hash = re.sub(r"^-+", "", path_hash)
    path_hash = re.sub(r"-+$", "", path_hash)
    path_hash = re.sub(r"-+", "-", path_hash)
    jsonl = os.path.join(config_dir, "projects", path_hash, f"{sid}.jsonl")
    if req_log:
        with open(req_log, "a") as fh:
            status = "OK" if os.path.exists(jsonl) else f"MISSING {jsonl}"
            fh.write(f"RESUME {sid} {status}\n")


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def log(line):
    if req_log:
        with open(req_log, "a") as fh:
            fh.write(line + "\n")


_sdk_declared = False


def handle_control(msg):
    """Ack a control_request; returns the subtype."""
    global _sdk_declared
    request = msg.get("request", {})
    subtype = request.get("subtype")
    extra = request.get("model", "")
    if subtype == "initialize":
        servers = request.get("sdkMcpServers") or []
        if servers:
            _sdk_declared = True
        extra = "sdkMcpServers=" + ",".join(servers)
    log(f"CONTROL {subtype} {extra}")
    send({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": msg.get("request_id"),
            "response": {
                "models": [{"id": "mock-1", "name": "Mock Model"}],
                "currentModelId": "mock-1",
            },
        },
    })
    return subtype


def stream_text_turn(text, usage_in, usage_out):
    """One turn delivered via stream_event partials (the assistant message
    that follows must be IGNORED by the provider — its text differs)."""
    send({"type": "stream_event", "event": {
        "type": "message_start",
        "message": {"usage": {"input_tokens": usage_in, "output_tokens": 0}},
    }, "session_id": SESSION})
    send({"type": "stream_event", "event": {
        "type": "content_block_start", "index": 0,
        "content_block": {"type": "text", "text": ""},
    }, "session_id": SESSION})
    for chunk in text:
        send({"type": "stream_event", "event": {
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": chunk},
        }, "session_id": SESSION})
    send({"type": "stream_event", "event": {
        "type": "content_block_stop", "index": 0,
    }, "session_id": SESSION})
    send({"type": "stream_event", "event": {
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn"},
        "usage": {"input_tokens": usage_in, "output_tokens": usage_out},
    }, "session_id": SESSION})
    send({"type": "stream_event", "event": {"type": "message_stop"},
          "session_id": SESSION})


send({"type": "system", "subtype": "init", "session_id": SESSION, "model": "mock-1"})

# NOTE: every stdin consumer goes through this queue, fed by one daemon
# reader thread. select() on sys.stdin works on sockets only — on Windows
# it dies with WinError 10038 on pipes — and mixing readline() sites would
# let one pre-buffer lines the other's interrupt wait must see.
_lines = queue.Queue()


def _reader():
    for line in sys.stdin:
        _lines.put(line)
    _lines.put(None)  # EOF: provider closed stdin / killed the process


threading.Thread(target=_reader, daemon=True).start()


# ---------------------------------------------------------------------------
# SDK-MCP client side (mcp_message control frames, like the real CLI)
# ---------------------------------------------------------------------------

_mcp_req = 0
_mcp_handshake_done = False


def mcp_send(message):
    """Send one MCP JSON-RPC message as an mcp_message control_request
    WITHOUT waiting for the response. The real CLI dispatches all of a
    parallel batch's tools/calls up front (observed on the 2.156.0 wire),
    so multi-call scenarios send first and await later."""
    global _mcp_req
    _mcp_req += 1
    request_id = f"mcp_{_mcp_req}"
    send({
        "type": "control_request",
        "request_id": request_id,
        "request": {
            "subtype": "mcp_message",
            "server_name": MCP_SERVER,
            "message": message,
        },
    })
    log(f"MCP-SEND {message.get('method')}")
    return request_id


def mcp_await(request_id):
    """Wait for the control_response matching request_id; returns the
    mcp_response. Inbound control_requests (interrupt & co) are serviced
    meanwhile."""
    while True:
        line = _lines.get()
        if line is None:
            sys.exit(0)
        msg = json.loads(line)
        kind = msg.get("type")
        if kind == "control_request":
            handle_control(msg)
            continue
        if kind == "control_response" and \
                msg.get("response", {}).get("request_id") == request_id:
            response = msg["response"]
            if response.get("subtype") == "error":
                raise RuntimeError(f"mcp_message rejected: {response}")
            return response.get("response", {}).get("mcp_response", {})
        # Anything else while parked (cannot happen with the provider):
        # ignore.


def mcp_rpc(message):
    """Send one MCP JSON-RPC message as an mcp_message control_request and
    wait for the provider's control_response; returns the mcp_response."""
    return mcp_await(mcp_send(message))


def mcp_handshake():
    """The real CLI handshakes the SDK MCP server when the first prompt
    arrives (initialize -> notifications/initialized -> tools/list)."""
    global _mcp_handshake_done
    if _mcp_handshake_done or not _sdk_declared:
        return
    _mcp_handshake_done = True
    mcp_rpc({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "leader-bridge", "version": "1.0.0"},
        },
    })
    mcp_rpc({"jsonrpc": "2.0", "method": "notifications/initialized"})
    listed = mcp_rpc({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
    tools = [t.get("name", "") for t in listed.get("result", {}).get("tools", [])]
    log(f"MCP tools/list {','.join(tools)}")


def mcp_result_text(response):
    blocks = response.get("result", {}).get("content", [])
    return "\n".join(b.get("text", "") for b in blocks if b.get("type") == "text")


def mcp_call_tool(call_id, name, arguments):
    """Dispatch one tools/call and park until the provider resolves it;
    the real CLI then re-lists tools. Returns the result text."""
    response = mcp_rpc({
        "jsonrpc": "2.0", "id": call_id, "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    })
    mcp_rpc({"jsonrpc": "2.0", "id": call_id + 100, "method": "tools/list"})
    return mcp_result_text(response)


def send_tool_result_echo(tool_use_id, text):
    """The user-line tool_result echo the real CLI persists after an MCP
    call resolves."""
    send({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": [{"type": "text", "text": text}],
                "is_error": False,
            }],
        },
        "session_id": SESSION,
    })


while True:
    line = _lines.get()
    if line is None:
        break
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    kind = msg.get("type")
    if kind == "control_request":
        handle_control(msg)
        continue
    if kind != "user":
        continue
    mcp_handshake()
    content = msg.get("message", {}).get("content", [])
    text = " ".join(b.get("text", "") for b in content if b.get("type") == "text")
    if "slow" in text:
        # Stay in-flight until an interrupt control_request arrives (tests
        # abort mid-generation). On interrupt: ack + emit the turn's result.
        deadline = time.time() + 30
        interrupted = False
        while time.time() < deadline and not interrupted:
            try:
                line2 = _lines.get(timeout=0.1)
            except queue.Empty:
                continue
            if line2 is None:
                break
            msg2 = json.loads(line2)
            if msg2.get("type") != "control_request":
                continue
            if handle_control(msg2) == "interrupt":
                send({
                    "type": "result", "subtype": "success", "is_error": True,
                    "result": "interrupted", "session_id": SESSION,
                    "usage": {"input_tokens": 1, "output_tokens": 1},
                    "total_cost_usd": 0,
                })
                interrupted = True
        if not interrupted:
            send({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "echo: " + text}],
                    "usage": {"input_tokens": 5, "output_tokens": 3},
                },
                "session_id": SESSION,
            })
            send({
                "type": "result", "subtype": "success", "is_error": False,
                "result": "done", "session_id": SESSION,
                "usage": {"input_tokens": 5, "output_tokens": 3},
                "total_cost_usd": 0,
            })
    elif "use-echo-tool" in text:
        send({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "mcp__tack__echo",
                     "input": {"text": "hello-from-tool"}}
                ],
                "usage": {"input_tokens": 10, "output_tokens": 5},
            },
            "session_id": SESSION,
        })
        # The tool boundary: the provider returned with ToolUse; the CLI
        # dispatches the MCP call and parks until it resolves.
        result_text = mcp_call_tool(2, "echo", {"text": "hello-from-tool"})
        send_tool_result_echo("toolu_1", result_text)
        # Continuation streams like any other turn (the trailing completed
        # assistant message is a duplicate the provider must ignore).
        stream_text_turn(["tool said: pong"], 20, 8)
        send({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "IGNORED-DUPLICATE"}],
                "usage": {"input_tokens": 20, "output_tokens": 8},
            },
            "session_id": SESSION,
        })
        send({
            "type": "result", "subtype": "success", "is_error": False,
            "result": "done", "session_id": SESSION,
            "usage": {"input_tokens": 30, "output_tokens": 13},
            "total_cost_usd": 0,
        })
    elif "parallel-tool" in text:
        # PARALLEL tool calls: the real CLI (2.156.0) reuses ONE content
        # index for every tool_use block in a batched message and emits a
        # single content_block_stop for all of them. The provider must
        # stop (and park) EVERY block carrying that index — a first-match
        # stop parks only the first call and the second tool result later
        # fails sync ("unknown call") into a transcript respawn.
        send({"type": "stream_event", "event": {
            "type": "message_start",
            "message": {"usage": {"input_tokens": 11, "output_tokens": 0}},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_p1",
                              "name": "mcp__tack__echo", "input": {}},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta",
                      "partial_json": "{\"text\":\"one\"}"},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_p2",
                              "name": "mcp__tack__echo", "input": {}},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta",
                      "partial_json": "{\"text\":\"two\"}"},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_stop", "index": 0,
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use"},
            "usage": {"input_tokens": 11, "output_tokens": 9},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {"type": "message_stop"},
              "session_id": SESSION})
        # Two parked tools/calls, dispatched UP FRONT in block order (the
        # real CLI sends the whole parallel batch before waiting on any
        # response — observed on the 2.156.0 wire).
        req_one = mcp_send({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                            "params": {"name": "echo", "arguments": {"text": "one"}}})
        req_two = mcp_send({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                            "params": {"name": "echo", "arguments": {"text": "two"}}})
        result_one = mcp_await(req_one)
        result_two = mcp_await(req_two)
        # The real CLI re-lists tools after a call resolves.
        mcp_rpc({"jsonrpc": "2.0", "id": 102, "method": "tools/list"})
        mcp_rpc({"jsonrpc": "2.0", "id": 103, "method": "tools/list"})
        # Stale echo: the real CLI re-yields the completed message after
        # the calls resolve, split per block.
        for tool_use_id, text_in in (("toolu_p1", "one"), ("toolu_p2", "two")):
            send({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": tool_use_id,
                         "name": "mcp__tack__echo", "input": {"text": text_in}}
                    ],
                    "usage": {"input_tokens": 11, "output_tokens": 9},
                },
                "session_id": SESSION,
            })
        send_tool_result_echo("toolu_p1", mcp_result_text(result_one))
        send_tool_result_echo("toolu_p2", mcp_result_text(result_two))
        stream_text_turn(["tool said: pong"], 24, 5)
        send({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "IGNORED-DUPLICATE"}],
                "usage": {"input_tokens": 24, "output_tokens": 5},
            },
            "session_id": SESSION,
        })
        send({
            "type": "result", "subtype": "success", "is_error": False,
            "result": "done", "session_id": SESSION,
            "usage": {"input_tokens": 35, "output_tokens": 14},
            "total_cost_usd": 0,
        })
    elif "parallel-tangled" in text:
        # TANGLED parallel calls (observed on the 2.156.0 wire with
        # deepseek-v4-pro): BOTH tool_use blocks start up front on the
        # SAME index, then both input_json streams arrive on that index
        # (every delta routes to the last-started block), one shared stop.
        # The streamed arguments are unrecoverable — call 1 sees nothing,
        # call 2 sees both payloads concatenated. Only the tools/call MCP
        # frames (dispatched from the CLI's complete assistant message)
        # carry the real arguments; the provider must adopt them.
        send({"type": "stream_event", "event": {
            "type": "message_start",
            "message": {"usage": {"input_tokens": 11, "output_tokens": 0}},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_t1",
                              "name": "mcp__tack__echo", "input": {}},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_t2",
                              "name": "mcp__tack__echo", "input": {}},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta",
                      "partial_json": "{\"text\":\"one\"}"},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta",
                      "partial_json": "{\"text\":\"two\"}"},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_stop", "index": 0,
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use"},
            "usage": {"input_tokens": 11, "output_tokens": 9},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {"type": "message_stop"},
              "session_id": SESSION})
        req_one = mcp_send({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                            "params": {"name": "echo", "arguments": {"text": "one"}}})
        req_two = mcp_send({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                            "params": {"name": "echo", "arguments": {"text": "two"}}})
        result_one = mcp_await(req_one)
        result_two = mcp_await(req_two)
        mcp_rpc({"jsonrpc": "2.0", "id": 102, "method": "tools/list"})
        for tool_use_id, text_in in (("toolu_t1", "one"), ("toolu_t2", "two")):
            send({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": tool_use_id,
                         "name": "mcp__tack__echo", "input": {"text": text_in}}
                    ],
                    "usage": {"input_tokens": 11, "output_tokens": 9},
                },
                "session_id": SESSION,
            })
        send_tool_result_echo("toolu_t1", mcp_result_text(result_one))
        send_tool_result_echo("toolu_t2", mcp_result_text(result_two))
        stream_text_turn(["tool said: pong"], 24, 5)
        send({
            "type": "result", "subtype": "success", "is_error": False,
            "result": "done", "session_id": SESSION,
            "usage": {"input_tokens": 35, "output_tokens": 14},
            "total_cost_usd": 0,
        })
    elif "retry-thinking" in text:
        # API RETRY mid-turn (observed with deepseek-v4-pro): the first
        # attempt's thinking block never stops; a second message_start
        # restarts the content indices. The provider must finalize the
        # abandoned block (ThinkingEnd) — otherwise it dangles forever
        # and later index-0 deltas misroute into the retry's block.
        send({"type": "stream_event", "event": {
            "type": "message_start",
            "message": {"usage": {"input_tokens": 7, "output_tokens": 0}},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "thinking", "thinking": ""},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "thinking_delta", "thinking": "first attempt"},
        }, "session_id": SESSION})
        # Retry: new message_start, indices restart at 0.
        send({"type": "stream_event", "event": {
            "type": "message_start",
            "message": {"usage": {"input_tokens": 7, "output_tokens": 0}},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "thinking", "thinking": ""},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "thinking_delta", "thinking": "second attempt"},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_stop", "index": 0,
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_start", "index": 1,
            "content_block": {"type": "text", "text": ""},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_delta", "index": 1,
            "delta": {"type": "text_delta", "text": "retried answer"},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_stop", "index": 1,
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"input_tokens": 7, "output_tokens": 6},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {"type": "message_stop"},
              "session_id": SESSION})
        send({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "IGNORED-DUPLICATE"}],
                "usage": {"input_tokens": 7, "output_tokens": 6},
            },
            "session_id": SESSION,
        })
        send({
            "type": "result", "subtype": "success", "is_error": False,
            "result": "done", "session_id": SESSION,
            "usage": {"input_tokens": 14, "output_tokens": 6},
            "total_cost_usd": 0,
        })
    elif "tool-input-at-start" in text:
        # Tool input delivered ENTIRELY at content_block_start (no
        # input_json_delta stream at all — some model/CLI combos).
        # The provider must seed arguments from content_block.input.
        send({"type": "stream_event", "event": {
            "type": "message_start",
            "message": {"usage": {"input_tokens": 11, "output_tokens": 0}},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_s1",
                              "name": "mcp__tack__echo",
                              "input": {"text": "seeded"}},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_stop", "index": 0,
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use"},
            "usage": {"input_tokens": 11, "output_tokens": 9},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {"type": "message_stop"},
              "session_id": SESSION})
        result_text = mcp_call_tool(2, "echo", {"text": "seeded"})
        send({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_s1",
                     "name": "mcp__tack__echo", "input": {"text": "seeded"}}
                ],
                "usage": {"input_tokens": 11, "output_tokens": 9},
            },
            "session_id": SESSION,
        })
        send_tool_result_echo("toolu_s1", result_text)
        stream_text_turn(["tool said: pong"], 24, 5)
        send({
            "type": "result", "subtype": "success", "is_error": False,
            "result": "done", "session_id": SESSION,
            "usage": {"input_tokens": 35, "output_tokens": 14},
            "total_cost_usd": 0,
        })
    elif "stream-hi" in text:
        # Incremental path: content arrives ONLY via stream_event partials;
        # the trailing assistant message must be ignored.
        stream_text_turn(["stream-", "echo: ", "hi"], 7, 2)
        send({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "IGNORED-DUPLICATE"}],
                "usage": {"input_tokens": 7, "output_tokens": 2},
            },
            "session_id": SESSION,
        })
        send({
            "type": "result", "subtype": "success", "is_error": False,
            "result": "done", "session_id": SESSION,
            "usage": {"input_tokens": 30, "output_tokens": 9},
            "modelUsage": {"mock-stream": {"contextWindow": 1048576,
                                           "maxOutputTokens": 32768}},
            "total_cost_usd": 0,
        })
    elif "stream-tool" in text:
        # Tool call delivered via stream_event partials (input_json deltas).
        send({"type": "stream_event", "event": {
            "type": "message_start",
            "message": {"usage": {"input_tokens": 11, "output_tokens": 0}},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_9",
                              "name": "mcp__tack__echo", "input": {}},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta",
                      "partial_json": "{\"text\":"},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta",
                      "partial_json": "\"streamed\"}"},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "content_block_stop", "index": 0,
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use"},
            "usage": {"input_tokens": 11, "output_tokens": 6},
        }, "session_id": SESSION})
        send({"type": "stream_event", "event": {"type": "message_stop"},
              "session_id": SESSION})
        # The tool boundary: the provider returned at message_stop; the
        # CLI dispatches the MCP call and parks until it resolves.
        result_text = mcp_call_tool(2, "echo", {"text": "streamed"})
        # The real CLI persists + yields the completed assistant message
        # AFTER the tool call resolves (SDK: "always yields assistant
        # messages after streaming") — an exact echo of the blocks already
        # streamed. The provider returned at message_stop, so this echo is
        # read by the NEXT turn's event loop and must be skipped as a
        # duplicate, not re-parked as a fresh tool call.
        send({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_9",
                     "name": "mcp__tack__echo", "input": {"text": "streamed"}}
                ],
                "usage": {"input_tokens": 11, "output_tokens": 6},
            },
            "session_id": SESSION,
        })
        send_tool_result_echo("toolu_9", result_text)
        stream_text_turn(["tool said: pong"], 21, 4)
        send({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "IGNORED-DUPLICATE"}],
                "usage": {"input_tokens": 21, "output_tokens": 4},
            },
            "session_id": SESSION,
        })
        send({
            "type": "result", "subtype": "success", "is_error": False,
            "result": "done", "session_id": SESSION,
            "usage": {"input_tokens": 32, "output_tokens": 10},
            "total_cost_usd": 0,
        })
    elif "auth-fail" in text:
        # Exact CLI 2.156.0 capture (unauthenticated turn): an assistant
        # message carries the human-readable text, then the result fails
        # with subtype error_during_execution and an `errors` STRING
        # ARRAY. `errors_info` never existed on the wire.
        auth_error = ("Authentication required. Please use /login "
                      "command to sign in to your account")
        send({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": auth_error}],
                "usage": {"input_tokens": 0, "output_tokens": 0},
            },
            "session_id": SESSION,
        })
        send({
            "type": "result", "subtype": "error_during_execution",
            "is_error": True, "errors": [auth_error],
            "permission_denials": [], "session_id": SESSION,
            "usage": {"input_tokens": 0, "output_tokens": 0},
            "total_cost_usd": 0,
        })
    else:
        send({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "echo: " + text}],
                "usage": {"input_tokens": 5, "output_tokens": 3},
            },
            "session_id": SESSION,
        })
        send({
            "type": "result", "subtype": "success", "is_error": False,
            "result": "done", "session_id": SESSION,
            "usage": {"input_tokens": 5, "output_tokens": 3},
            "total_cost_usd": 0,
        })
