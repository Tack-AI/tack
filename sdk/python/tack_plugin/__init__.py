"""tack-plugin: SDK for tack-RPC v3 plugins.

Protocol types (tack_plugin.types) are generated from
protocol/tack-rpc.openrpc.json. stdout is the RPC bus — never print()
from a plugin; use ``cx.host.log()`` / ``cx.host.warn()``.
"""

from .peer import (
    CANCEL_METHOD,
    ERR_INTERNAL,
    ERR_METHOD_NOT_FOUND,
    ERR_PARSE,
    ERR_PLUGIN_UNAVAILABLE,
    ERR_REQUEST_TIMEOUT,
    JsonRpcPeer,
    PeerError,
)
from .plugin import (
    ERR_CAPABILITY_NOT_GRANTED,
    ERR_INVALID_PARAMS,
    PROTOCOL_VERSION,
    Cx,
    Host,
    Plugin,
    PluginError,
    allow,
    deny,
    error_output,
    rewrite,
    text_block,
    text_output,
)
from .provider import ProviderEvents, ProviderStreamCx

__all__ = [
    "CANCEL_METHOD",
    "Cx",
    "ERR_CAPABILITY_NOT_GRANTED",
    "ERR_INTERNAL",
    "ERR_INVALID_PARAMS",
    "ERR_METHOD_NOT_FOUND",
    "ERR_PARSE",
    "ERR_PLUGIN_UNAVAILABLE",
    "ERR_REQUEST_TIMEOUT",
    "Host",
    "JsonRpcPeer",
    "PROTOCOL_VERSION",
    "PeerError",
    "Plugin",
    "PluginError",
    "ProviderEvents",
    "ProviderStreamCx",
    "allow",
    "deny",
    "error_output",
    "rewrite",
    "text_block",
    "text_output",
]
