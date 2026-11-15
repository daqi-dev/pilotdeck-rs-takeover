import assert from "node:assert/strict";
import test from "node:test";
import { PermissionRuntime } from "../../src/permission/index.js";
import { ToolRuntime } from "../../src/tool/execution/ToolRuntime.js";
import { PilotDeckToolRuntimeError } from "../../src/tool/protocol/errors.js";
import { ToolRegistry } from "../../src/tool/registry/ToolRegistry.js";

test("a rejected validation produces a paired result and does not execute", async () => {
  const registry = new ToolRegistry();
  let executed = false;
  registry.register({
    name: "private_path", description: "Validate a confined path", kind: "custom",
    inputSchema: { type: "object", properties: {} },
    isReadOnly: () => true, isConcurrencySafe: () => true,
    validateInput: async () => { throw new PilotDeckToolRuntimeError("invalid_tool_input", "Path is outside this session"); },
    execute: async () => { executed = true; return { content: [] }; },
  });
  const result = await new ToolRuntime(registry, new PermissionRuntime()).execute(
    { id: "rejected-call", name: "private_path", input: {} },
    {
      sessionId: "a", turnId: "turn", cwd: process.cwd(), permissionMode: "bypassPermissions",
      permissionContext: {
        mode: "bypassPermissions", cwd: process.cwd(), additionalWorkingDirectories: [],
        canPrompt: false, bypassAvailable: true, rules: { allow: [], deny: [], ask: [] },
      },
    },
  );
  assert.equal(result.type, "error");
  assert.equal(result.toolCallId, "rejected-call");
  if (result.type === "error") assert.equal(result.error.code, "invalid_tool_input");
  assert.equal(executed, false);
});
