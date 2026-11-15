import type { SandboxPort, WorkspaceSandboxPolicyResolver } from "./SandboxPort.js";
import type { SubprocessFileRequest, SubprocessPort, SubprocessRequest, SubprocessResult } from "./SubprocessPort.js";

export type CreateNodeSandboxedSubprocessPortOptions = {
  sandbox: SandboxPort;
  subprocess: Pick<SubprocessPort, "executeFile">;
  resolvePolicy: WorkspaceSandboxPolicyResolver;
  platform?: NodeJS.Platform;
};

/** Routes both shell-string and direct executable requests through one policy. */
export function createNodeSandboxedSubprocessPort(options: CreateNodeSandboxedSubprocessPortOptions): SubprocessPort {
  const platform = options.platform ?? process.platform;
  return {
    supportsFileMtimeSort: options.sandbox.supportsFileMtimeSort,
    async execute(request: SubprocessRequest): Promise<SubprocessResult> {
      const executable = platform === "win32" ? process.env.ComSpec ?? "cmd.exe" : "/bin/sh";
      const args = platform === "win32" ? ["/d", "/s", "/c", request.command] : ["-c", request.command];
      const prepared = await options.sandbox.prepare({
        executable,
        args,
        cwd: request.cwd,
        env: request.env ?? process.env,
        policy: options.resolvePolicy({ workspaceRoot: request.cwd }),
        signal: request.signal,
      });
      return options.subprocess.executeFile!({ ...request, ...prepared });
    },
    async executeFile(request: SubprocessFileRequest): Promise<SubprocessResult> {
      const prepared = await options.sandbox.prepare({
        executable: request.executable,
        args: request.args,
        cwd: request.cwd,
        env: request.env ?? process.env,
        policy: options.resolvePolicy({ workspaceRoot: request.cwd }),
        signal: request.signal,
      });
      return options.subprocess.executeFile!({ ...request, ...prepared });
    },
  };
}
