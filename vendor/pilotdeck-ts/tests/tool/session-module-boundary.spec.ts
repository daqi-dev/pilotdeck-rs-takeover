import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import ts from "typescript";
import { retargetExecutionSnapshot } from "../../src/sandbox/SessionExecutionStorage.js";
import { getPilotProjectChatDir } from "../../src/pilot/index.js";
import { createProjectSessionForkPort } from "../../src/session/index.js";
import type { AgentTranscriptEntry } from "../../src/session/transcript/TranscriptEntry.js";

test("core imports and public SDK/wire types keep the execution provider removable", async () => {
  const root = fileURLToPath(new URL("../../", import.meta.url));
  const inspect = async (dir: string) => {
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      const path = join(dir, entry.name);
      if (entry.isDirectory()) { if (!relative(root, path).startsWith("src/sandbox")) await inspect(path); continue; }
      if (!entry.name.endsWith(".ts")) continue;
      const source = ts.createSourceFile(path, await readFile(path, "utf8"), ts.ScriptTarget.Latest, true);
      const visit = (node: ts.Node) => {
        if ((ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) && node.moduleSpecifier && ts.isStringLiteral(node.moduleSpecifier)) {
          assert.doesNotMatch(node.moduleSpecifier.text, /sandbox\/nsjail/, path);
        }
        if (ts.isCallExpression(node) && node.arguments[0] && ts.isStringLiteral(node.arguments[0])) assert.doesNotMatch(node.arguments[0].text, /sandbox\/nsjail/, path);
        ts.forEachChild(node, visit);
      };
      visit(source);
    }
  };
  await inspect(join(root, "src"));
  for (const path of ["packages/sdk/src/types.ts", "packages/sdk/src/gateway-protocol.ts", "src/gateway/protocol/types.ts", "src/gateway/protocol/frames.ts"]) {
    const source = ts.createSourceFile(path, await readFile(join(root, path), "utf8"), ts.ScriptTarget.Latest, true);
    const visit = (node: ts.Node) => {
      if (ts.isPropertySignature(node) || ts.isPropertyDeclaration(node)) assert.doesNotMatch(node.name.getText(source), /nsjail|sessionExecutionProvider|sessionIsolationPolicy|sessionExecutionStorageRoot/i, path);
      ts.forEachChild(node, visit);
    };
    visit(source);
  }
});

test("native fork applies execution-storage retargeting to copied child transcripts", async () => {
  const root = await mkdtemp(join(tmpdir(), "pd-fork-retarget-"));
  try {
    const chat = getPilotProjectChatDir(root, root);
    const childDir = join(chat, "a", "subagents");
    await mkdir(childDir, { recursive: true });
    const executionRoot = join(root, "sessions");
    const sourcePath = join(executionRoot, "YQ", "spill", "result.txt");
    const targetPath = join(executionRoot, "Yg", "spill", "result.txt");
    const entry = { type: "tool_result_message", sessionId: "child", message: { role: "user", content: [{ type: "tool_result_reference", path: sourcePath }] } } as AgentTranscriptEntry;
    await writeFile(join(childDir, "child.jsonl"), JSON.stringify(entry) + "\n");
    await createProjectSessionForkPort().fork({ projectRoot: root, pilotHome: root, sourceSessionId: "a", targetSessionId: "b", entries: [],
      transformAuxiliaryEntry: (value) => retargetExecutionSnapshot(value, executionRoot, "a", "b") });
    const copied = await readFile(join(chat, "b", "subagents", "child.jsonl"), "utf8");
    assert.ok(copied.includes(targetPath), copied);
    assert.ok(!copied.includes(sourcePath), copied);
  } finally { await rm(root, { recursive: true, force: true }); }
});
