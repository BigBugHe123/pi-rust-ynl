---
name: pi-mcp-management
description: Connect, create, test, disable, or remove Pi MCP services, data sources, and tools.
---

# Manage MCP connections

Reply in the user's language. Infer paths and config from the session. Ask in plain language only when a missing detail or ambiguous name affects the result. Explain what changed and how to use it.

Default to the current session project. Use PI_CODING_AGENT_DIR for global installs only when explicitly requested. If the project is unavailable or installation fails, explain and ask where to proceed. Check existing names and scope before editing.

After a successful change, tell the user to wait for the reply to finish, then send /reload in the same chat. Verify after reload and report only what you verified.

Identify the service the user wants. Connect an existing service if available. For sign-in, guide the user through the service's login flow. To write a local server, use the bundled example below.

Merge entries under mcpServers in the trusted project's .pi/mcp.json or the global agent directory's mcp.json. Preserve existing entries. For stdio, use a command available on the backend and an args array. For HTTP, use the MCP URL. Supply credentials via environment variables or service authentication.

```json
{
  "mcpServers": {
    "local-docs": { "command": "node", "args": ["/absolute/path/to/server.mjs"] },
    "remote-docs": { "url": "https://docs.example/mcp" }
  }
}
```

To disconnect, remove the defining entry and check for the same name in the other scope. To disable a global server only in this project, use a project override with enabled set to false. Uninstall the server program only when requested and unused elsewhere. After reload, verify in Pi Web Settings > MCP or CLI /mcp.

## Minimal stdio server

Save as server.mjs and put its absolute backend path in args. Requires Node.js and uses its standard library.

```js
import { createInterface } from "node:readline";

const tool = {
  name: "hello",
  description: "Return a greeting",
  inputSchema: { type: "object", properties: {}, additionalProperties: false },
};

function handle(method, params) {
  switch (method) {
    case "initialize":
      return {
        protocolVersion: "2025-06-18",
        capabilities: { tools: {} },
        serverInfo: { name: "local-docs", version: "1.0.0" },
      };
    case "ping":
      return {};
    case "tools/list":
      return { tools: [tool] };
    case "tools/call":
      if (params?.name !== tool.name) {
        throw { code: -32602, message: "Unknown tool" };
      }
      return { content: [{ type: "text", text: "Hello from MCP!" }] };
    default:
      throw { code: -32601, message: "Method not found" };
  }
}

for await (const line of createInterface({ input: process.stdin, crlfDelay: Infinity })) {
  let request;
  try {
    request = JSON.parse(line);
  } catch {
    process.stdout.write(JSON.stringify({
      jsonrpc: "2.0", id: null, error: { code: -32700, message: "Parse error" },
    }) + "\n");
    continue;
  }
  if (request?.id === undefined) continue;
  const response = { jsonrpc: "2.0", id: request.id };
  try {
    response.result = handle(request.method, request.params);
  } catch (error) {
    response.error = { code: error.code ?? -32603, message: error.message };
  }
  process.stdout.write(JSON.stringify(response) + "\n");
}
```

Test initialize, notifications/initialized, tools/list, and tools/call. Requests get the same id back. Notifications get no reply. Call hello with {"name":"hello","arguments":{}}. Reserve stdout for protocol messages and use stderr for logs.
