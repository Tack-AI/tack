// protected-paths: block tool calls that touch sensitive paths
// (.env, .git/, node_modules/, key material) via intercept.tool_call.
// Install: tack ext install <this-dir>, or copy to ~/.tack/agent/extensions/.

import readline from "node:readline";

const rl = readline.createInterface({ input: process.stdin, terminal: false });
const send = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");

const PROTECTED_PATTERNS = [
  /(^|[\\/])\.env(\.[a-z]+)?$/i,
  /(^|[\\/])\.git([\\/]|$)/i,
  /(^|[\\/])node_modules([\\/]|$)/i,
  /\.(pem|key|p12|pfx|jks|keystore)$/i,
  /(^|[\\/])id_(rsa|ed25519|ecdsa|dsa)(\.pub)?$/i,
  /(^|[\\/])\.aws([\\/]|$)/i,
];
const MUTATING_TOOLS = new Set(["write", "edit", "bash"]);

function isProtected(path) {
  if (!path) return false;
  return PROTECTED_PATTERNS.some((re) => re.test(path));
}

function violation(toolName, args) {
  if (!MUTATING_TOOLS.has(toolName)) return undefined;
  // Direct path parameters (write/edit/read-like paths).
  const direct = args?.path ?? args?.filePath ?? args?.target;
  if (isProtected(direct)) return direct;
  // Bash commands: scan for protected-looking tokens.
  if (toolName === "bash" && typeof args?.command === "string") {
    for (const token of args.command.split(/\s+/)) {
      if (isProtected(token)) return token;
    }
  }
  return undefined;
}

rl.on("line", (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg.type === "event" && msg.event === "initialize") {
    send({
      type: "event",
      event: "register",
      payload: { name: "protected-paths", subscriptions: ["tool_call"] },
    });
    return;
  }
  if (msg.type === "event" && msg.event === "shutdown") {
    process.exit(0);
  }
  if (msg.type === "request" && msg.method === "intercept.tool_call") {
    const { toolName, arguments: args } = msg.params;
    const hit = violation(toolName, args ?? {});
    send({
      type: "response",
      id: msg.id,
      result: hit
        ? { action: "deny", reason: `protected path blocked by protected-paths: ${hit}` }
        : { action: "allow" },
    });
    return;
  }
  if (msg.type === "request") {
    send({ type: "response", id: msg.id, error: `unknown method ${msg.method}` });
  }
});
