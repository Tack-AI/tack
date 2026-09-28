// @tack/plugin — SDK for tack-RPC v3 plugins (TypeScript/JavaScript).
// The protocol types (types.d.ts) are generated from
// protocol/tack-rpc.openrpc.json.

export { createPeer, PeerError } from "./peer.js";
export {
  plugin,
  textBlock,
  textOutput,
  errorOutput,
  allow,
  deny,
  rewrite,
  PROTOCOL_VERSION,
} from "./plugin.js";
