import type {
  CodeRuntimePort,
  CodeRuntimeRequest,
  CodeRuntimeResult,
} from "./CodeRuntimePort.js";
import type { SandboxPort, WorkspaceSandboxPolicyResolver } from "./SandboxPort.js";

export type CreateNodeSandboxedCodeRuntimePortOptions = {
  sandbox: SandboxPort;
  runtime: CodeRuntimePort;
  resolvePolicy: WorkspaceSandboxPolicyResolver;
  probeCwd?: string;
};

/** Routes model-authored code through the same exact-argv sandbox as shell. */
export function createNodeSandboxedCodeRuntimePort(
  options: CreateNodeSandboxedCodeRuntimePortOptions,
): CodeRuntimePort {
  return {
    enforcesSandbox: true,
    async resolveExecutable(candidates, env, signal) {
      for (const executable of candidates) {
        signal?.throwIfAborted();
        try {
          const command = await options.sandbox.prepare({
            executable, args: ["--version"], cwd: options.probeCwd ?? process.cwd(),
            env: env ?? process.env,
            policy: options.resolvePolicy({ workspaceRoot: options.probeCwd ?? process.cwd() }), signal,
          });
          const result = await options.runtime.run({ ...command, signal, timeoutMs: 5_000, stdoutMaxBytes: 4096, stderrMaxBytes: 4096 });
          if (result.exitCode === 0 && !result.cancelled && !result.timedOut) return executable;
        } catch {
          signal?.throwIfAborted();
        }
      }
      return undefined;
    },
    run: async (request): Promise<CodeRuntimeResult> => {
      const prepared = await options.sandbox.prepare({
        executable: request.executable,
        args: request.args,
        cwd: request.cwd,
        env: request.env,
        policy: options.resolvePolicy({ workspaceRoot: request.cwd }),
        signal: request.signal,
      });
      return options.runtime.run({
        ...request,
        executable: prepared.executable,
        args: prepared.args,
        cwd: prepared.cwd,
        env: prepared.env,
      });
    },
    dispose: () => options.runtime.dispose?.() ?? Promise.resolve(),
  };
}
