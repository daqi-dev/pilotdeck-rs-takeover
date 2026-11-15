import { spawn } from "node:child_process";
import {
  NodeShellCommandRunner,
  type PilotDeckCommandOptions,
  type PilotDeckCommandResult,
  type PilotDeckCommandRunner,
} from "../builtin/bash/commandRunner.js";

/** Host-independent subprocess request consumed by shell tools. */
export type SubprocessRequest = PilotDeckCommandOptions & { command: string };

/** Canonical result returned by a subprocess provider. */
export type SubprocessResult = PilotDeckCommandResult & {
  /** Present for direct executable requests that close because of a signal. */
  exitSignal?: NodeJS.Signals | null;
  stdoutTruncated?: boolean;
  stderrTruncated?: boolean;
};

/** Host-independent request for a program that must not be routed through a shell. */
export type SubprocessFileRequest = PilotDeckCommandOptions & {
  executable: string;
  args: readonly string[];
  /** Optional UTF-8 payload written to stdin before the process is observed. */
  stdin?: string;
};

/** DSH-style execution-world Definition for a foreground subprocess. */
export type SubprocessPort = {
  readonly supportsFileMtimeSort?: boolean;
  execute(request: SubprocessRequest): Promise<SubprocessResult>;
  /** Optional direct-executable path; consumers that need it must fail clearly when absent. */
  executeFile?(request: SubprocessFileRequest): Promise<SubprocessResult>;
};

/** Native provider adapter; the Node runner remains the source of shell semantics. */
export function createNodeSubprocessPort(
  runner: PilotDeckCommandRunner = new NodeShellCommandRunner(),
  options: { maxOutputBytes?: number } = {},
): SubprocessPort {
  if (options.maxOutputBytes !== undefined && (!Number.isSafeInteger(options.maxOutputBytes) || options.maxOutputBytes < 0)) {
    throw new RangeError("maxOutputBytes must be a non-negative integer");
  }
  return {
    execute: (request) => runner.run(request.command, request),
    executeFile: (request) => runNodeExecutable(request, options.maxOutputBytes),
  };
}

function runNodeExecutable(request: SubprocessFileRequest, maxOutputBytes?: number): Promise<SubprocessResult> {
  if (request.signal?.aborted) {
    return Promise.reject(new Error("Subprocess execution was aborted."));
  }

  const startedAt = Date.now();
  return new Promise((resolve, reject) => {
    let child: ReturnType<typeof spawn>;
    try {
      child = spawn(request.executable, request.args, {
        cwd: request.cwd,
        env: request.env,
        stdio: ["pipe", "pipe", "pipe"],
        detached: process.platform !== "win32",
        windowsHide: true,
      });
    } catch (error) {
      reject(error);
      return;
    }

    let stdout = "";
    let stderr = "";
    let stdoutBytes = 0;
    let stderrBytes = 0;
    let stdoutTruncated = false;
    let stderrTruncated = false;
    const retain = (chunk: string, used: number) => maxOutputBytes === undefined ? chunk
      : new TextDecoder().decode(Buffer.from(chunk).subarray(0, Math.max(0, maxOutputBytes - used)), { stream: true });
    let settled = false;
    let aborted = false;
    let timedOut = false;
    let timeout: ReturnType<typeof setTimeout> | undefined;
    const stop = () => {
      if (process.platform !== "win32" && child.pid) {
        try { process.kill(-child.pid, "SIGTERM"); } catch { /* process already exited */ }
        setTimeout(() => {
          try { process.kill(-child.pid!, "SIGKILL"); } catch { /* process already exited */ }
        }, 1_000).unref();
        return;
      }
      child.kill("SIGTERM");
      setTimeout(() => child.kill("SIGKILL"), 1_000).unref();
    };
    const cleanup = () => {
      if (timeout) clearTimeout(timeout);
      request.signal?.removeEventListener("abort", onAbort);
    };
    const finish = (result: SubprocessResult) => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve(result);
    };
    const fail = (error: Error) => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(error);
    };
    const onAbort = () => {
      aborted = true;
      stop();
    };

    timeout = setTimeout(() => {
      timedOut = true;
      stop();
    }, request.timeoutMs);
    child.stdout?.setEncoding("utf8");
    child.stderr?.setEncoding("utf8");
    child.stdout?.on("data", (chunk: string) => {
      const kept = retain(chunk, stdoutBytes);
      stdoutTruncated ||= Buffer.byteLength(kept) < Buffer.byteLength(chunk);
      stdoutBytes += Buffer.byteLength(kept);
      stdout += kept;
      try { if (kept) request.onStdout?.(kept); } catch { /* progress is best-effort */ }
    });
    child.stderr?.on("data", (chunk: string) => {
      const kept = retain(chunk, stderrBytes);
      stderrTruncated ||= Buffer.byteLength(kept) < Buffer.byteLength(chunk);
      stderrBytes += Buffer.byteLength(kept);
      stderr += kept;
      try { if (kept) request.onStderr?.(kept); } catch { /* progress is best-effort */ }
    });
    child.stdin?.end(request.stdin ?? "");
    child.on("error", (error) => fail(error));
    child.on("close", (exitCode, exitSignal) => {
      if (aborted) {
        fail(new Error("Subprocess execution was aborted."));
        return;
      }
      finish({
        exitCode,
        stdout,
        stderr,
        timedOut,
        durationMs: Date.now() - startedAt,
        exitSignal,
        ...(maxOutputBytes !== undefined ? { stdoutTruncated, stderrTruncated } : {}),
      });
    });
    request.signal?.addEventListener("abort", onAbort, { once: true });
    if (request.signal?.aborted) onAbort();
  });
}
