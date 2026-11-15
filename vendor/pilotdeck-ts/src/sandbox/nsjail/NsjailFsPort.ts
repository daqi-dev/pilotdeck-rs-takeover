import { isAbsolute, relative, resolve } from "node:path";
import type { FsPort, FsReadRangeResult, FsWriteTextResult, FsFileStat, FsDirectoryEntry } from "../../tool/execution-world/FsPort.js";
import type { SandboxPort } from "../../tool/execution-world/SandboxPort.js";
import { createNodeSubprocessPort } from "../../tool/execution-world/SubprocessPort.js";
import type { FileHistoryFsPort } from "../../session/filesystem/FileHistoryFsPort.js";
import type { InstructionStoragePort } from "../../context/instructions/InstructionStoragePort.js";
import type { ToolResultSpillPort } from "../../context/budget/ToolResultSpillPort.js";

/** Files are opened inside the mount namespace, including symlink resolution. */
export function createNsjailFsPort(sandbox: SandboxPort, workspace: string, additionalRoots: Readonly<Record<string, string>> = {}, executionSignal?: AbortSignal): FsPort & {
  fileHistoryFs: FileHistoryFsPort;
  instructionStorage: InstructionStoragePort;
  toolResultSpill: ToolResultSpillPort;
} {
  const subprocess = createNodeSubprocessPort(undefined, { maxOutputBytes: 90 * 1024 * 1024 });
  const roots = Object.fromEntries(Object.entries({ ...additionalRoots, "/workspace": workspace })
    .sort(([a], [b]) => b.length - a.length));
  const guestPath = (value: string) => {
    for (const [guest, host] of Object.entries(roots)) {
      const child = relative(resolve(host), resolve(value));
      if (child !== ".." && !child.startsWith("../") && !isAbsolute(child)) return `${guest}/${child}`;
    }
    throw new Error(`File path is outside the session workspace: ${value}`);
  };
  const hostPath = (value: string) => {
    for (const [guest, host] of Object.entries(roots)) {
      const child = relative(guest, value);
      if (child !== ".." && !child.startsWith("../") && !isAbsolute(child)) return resolve(host, child);
    }
    throw new Error("Canonical file path is outside the session storage");
  };
  async function call<T>(op: string, path: string, input: Record<string, unknown> = {}, signal?: AbortSignal): Promise<T> {
    signal = executionSignal ? signal ? AbortSignal.any([executionSignal, signal]) : executionSignal : signal;
    signal?.throwIfAborted();
    const prepared = await sandbox.prepare({
      executable: "node", args: ["-e", WORKER], cwd: workspace,
      env: { PATH: "/usr/local/bin:/usr/bin:/bin" },
      policy: { mode: "workspace-write", workspaceRoot: workspace }, signal,
    });
    const result = await subprocess.executeFile!({
      ...prepared, signal, timeoutMs: 30_000,
      stdin: JSON.stringify({ op, path: guestPath(path), ...input }),
    });
    signal?.throwIfAborted();
    if (result.exitCode !== 0) throw new Error(`Sandbox filesystem worker failed: ${result.stderr}`);
    const response = JSON.parse(result.stdout) as { value?: T; error?: { message: string; code?: string } };
    if (response.error) throw Object.assign(new Error(response.error.message), { code: response.error.code });
    return response.value as T;
  }
  const port: FsPort = {
    realpath: (path) => call<string>("realpath", path).then(hostPath),
    statMany: (paths, signal) => call<FsFileStat[]>("stats", workspace, { paths: paths.map(guestPath) }, signal),
    stat: (path, signal) => call<FsFileStat>("stat", path, {}, signal),
    readDirectory: (path, signal) => call<FsDirectoryEntry[]>("directory", path, {}, signal),
    readFile: (path, options = {}) => {
      // Keep path rejection synchronous for the FsPort containment contract.
      guestPath(path);
      return call<string>("read", path, {}, options.signal).then((encoded) => {
        const bytes = Buffer.from(encoded, "base64");
        return options.encoding === "utf8" ? bytes.toString("utf8") : new Uint8Array(bytes);
      });
    },
    readFileInRange: (path, startLine, limit, signal) => call<FsReadRangeResult>("range", path, { startLine, limit }, signal),
    writeText: (path, content, options = {}) => call<FsWriteTextResult>("write", path, { content, overwrite: options.allowOverwrite === true }, options.signal),
  };
  const fileHistoryFs: FileHistoryFsPort = {
    stat: async (path) => { const value = await port.stat(path); return { ...value, mode: value.mode ?? 0o600, isFile: () => value.kind === "file" }; },
    readFile: (async (path: string, encoding?: "utf8" | "utf-8") => {
      const bytes = Buffer.from(await port.readFile(path) as Uint8Array);
      return encoding ? bytes.toString("utf8") : bytes;
    }) as FileHistoryFsPort["readFile"],
    mkdir: (path) => call("mkdir", path),
    copyFile: (source, destination) => call("copy", source, { destination: guestPath(destination) }),
    chmod: (path, mode) => call("chmod", path, { mode }),
    unlink: (path) => call("unlink", path),
    rename: (source, destination) => call("rename", source, { destination: guestPath(destination) }),
    access: (path) => call("access", path),
    writeFile: (path, bytes) => call("bytes", path, { bytes: Buffer.from(bytes).toString("base64") }),
  };
  return {
    ...port, fileHistoryFs,
    instructionStorage: { readText: (path) => fileHistoryFs.readFile(path, "utf8"), readDirectory: (path) => port.readDirectory(path) },
    toolResultSpill: {
      writeTextIfAbsent: async (path, content) => {
        try { await port.writeText(path, content); return { created: true }; }
        catch (error) { if ((error as NodeJS.ErrnoException).code === "EEXIST") return { created: false }; throw error; }
      },
      copyFileIfAbsent: async (source, destination) => {
        try { await call("copy", source, { destination: guestPath(destination), exclusive: true }); return { created: true }; }
        catch (error) { if ((error as NodeJS.ErrnoException).code === "EEXIST") return { created: false }; throw error; }
      },
    },
  };
}

const WORKER = String.raw`
const fs = require('node:fs/promises');
const path = require('node:path');
const kind = s => s.isFile() ? 'file' : s.isDirectory() ? 'directory' : 'other';
(async () => {
  let input = ''; for await (const chunk of process.stdin) input += chunk;
  const r = JSON.parse(input); let value;
  if (r.op === 'realpath') {
    value = await fs.realpath(r.path);
  } else if (r.op === 'stats') {
    value = await Promise.all(r.paths.map(async p => { const s=await fs.stat(p); return { kind: kind(s), size:s.size, mtimeMs:s.mtimeMs }; }));
  } else if (r.op === 'stat') {
    const s = await fs.stat(r.path); value = { kind: kind(s), size: s.size, mtimeMs: s.mtimeMs, mode: s.mode };
  } else if (r.op === 'mkdir') {
    await fs.mkdir(r.path, { recursive: true });
  } else if (r.op === 'copy') {
    await fs.mkdir(path.dirname(r.destination), { recursive: true });
    await fs.copyFile(r.path, r.destination, r.exclusive ? 1 : 0);
  } else if (r.op === 'chmod') {
    await fs.chmod(r.path, r.mode);
  } else if (r.op === 'unlink') {
    await fs.unlink(r.path);
  } else if (r.op === 'rename') {
    await fs.rename(r.path, r.destination);
  } else if (r.op === 'access') {
    await fs.access(r.path);
  } else if (r.op === 'bytes') {
    await fs.writeFile(r.path, Buffer.from(r.bytes, 'base64'));
  } else if (r.op === 'directory') {
    value = (await fs.readdir(r.path, { withFileTypes: true })).map(s => ({ name: s.name, kind: kind(s) }));
  } else if (r.op === 'read') {
    value = (await fs.readFile(r.path)).toString('base64');
  } else if (r.op === 'range') {
    const s = await fs.stat(r.path); const b = await fs.readFile(r.path);
    if (b.includes(0)) throw new Error('File appears to be binary');
    const text = b.toString('utf8').replace(/^\uFEFF/, ''); const lines = text.split(/\r?\n/);
    const start = Math.max(1, r.startLine); const end = r.limit === undefined ? lines.length : start - 1 + Math.max(0, r.limit);
    const selected = lines.slice(start - 1, end); const content = selected.join('\n');
    const actual = selected.length ? start : Math.min(start, lines.length + 1);
    value = { content, ...(r.limit === undefined ? { fullContent: text } : {}), lineCount: selected.length,
      totalLines: lines.length, totalBytes: b.length, readBytes: Buffer.byteLength(content), mtimeMs: Math.floor(s.mtimeMs),
      startLine: actual, endLine: actual + selected.length - 1, truncated: start > 1 || end < lines.length };
  } else if (r.op === 'write') {
    const existing = await fs.stat(r.path).catch(e => { if (e.code === 'ENOENT') return null; throw e; });
    if (existing && (!existing.isFile() || !r.overwrite)) throw Object.assign(new Error('File conflict'), { code: 'EEXIST' });
    await fs.mkdir(path.dirname(r.path), { recursive: true });
    await fs.writeFile(r.path, r.content, { encoding: 'utf8', flag: r.overwrite ? 'w' : 'wx' });
    value = { action: existing ? 'overwritten' : 'created', mtimeMs: Math.floor((await fs.stat(r.path)).mtimeMs) };
  } else throw new Error('Unknown filesystem operation');
  process.stdout.write(JSON.stringify({ value }));
})().catch(e => process.stdout.write(JSON.stringify({ error: { message: e.message, code: e.code } })));
`;
