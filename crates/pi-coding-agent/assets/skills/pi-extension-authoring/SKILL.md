---
name: pi-extension-authoring
description: Create, install, test, update, or remove Pi plugins and extensions, including slash commands, tools, and event handlers.
---

# Manage Pi plugins

Reply in the user's language. Infer paths and config from the session. Ask in plain language only when a missing detail or ambiguous name affects the result. Explain what changed and how to use it.

Default to the current session project. Use PI_CODING_AGENT_DIR for global installs only when explicitly requested. If the project is unavailable or installation fails, explain and ask where to proceed. Check existing names and scope before editing.

After a successful change, tell the user to wait for the reply to finish, then send /reload in the same chat. Verify after reload and report only what you verified.

For packages, run pi install <source> -l from the project. Omit -l for explicitly requested global installs.

Identify whether the user wants a command, model tool, or event handler. Use .pi/extensions/ for project files or extensions/ under the global agent directory. Standalone CLI defaults to ~/.pi/agent if PI_CODING_AGENT_DIR is unset.

## JavaScript extension

Save as .pi/extensions/hello.js. Pi loads the default export with its extension API.

```js
export default function (pi) {
  pi.registerCommand("hello", {
    description: "Say hello",
    handler: async (args) => {
      const name = args.trim() || "world";
      pi.sendMessage(
        { customType: "hello", content: `Hello, ${name}!`, display: true },
        { triggerTurn: false },
      );
    },
  });
}
```

- registerCommand(name, { description, handler }) takes a name without / and handler(args, ctx). sendMessage with triggerTurn: false displays output without asking the model.
- registerTool takes name, label, description, parameters (JSON Schema), and execute(toolCallId, params, signal, onUpdate, ctx). Return { content: [{ type: "text", text: "done" }], details: {} }. Honor cancellation for long operations.
- pi.on(eventName, handler) subscribes to events such as session_start. Observe by returning nothing. Verify any event mutations against the running Pi version.

## Native extension

Read the bundled [native protocol reference](native-protocol.md) before implementation. Deploy the whole skill folder. Use any executable or interpreter available on the backend. Exchange one UTF-8 JSON-RPC 2.0 message per line over stdin/stdout. Flush responses and log to stderr.

Test with pi --extension <executable>. A package can declare pi.native in package.json, for example {"pi":{"native":[{"command":"bin/hello"}]}}. Command paths may be package-relative. On Windows, Pi adds .exe when the command has no suffix.

## Verification and removal

Check project trust and name collisions. After reload, inspect /extensions and run /hello Pi for the example. To remove a plugin, identify its source, then remove its standalone file, remove its explicit settings reference, or run pi remove <source> at the same scope (-l for project). Verify after reload.
