import { mkdirSync } from "node:fs";
import { join } from "node:path";
import type { PilotProxyConfig } from "../pilot/index.js";
import type { PilotDeckMcpServerSpec } from "../mcp/protocol/types.js";
import { sanitizeSessionIdForPath } from "../session/storage/ProjectSessionStorage.js";

export type BrowserUseSessionMcpSpecPreparationInput = {
  projectRoot: string;
  sessionKey: string;
  proxy?: PilotProxyConfig;
  specs: readonly PilotDeckMcpServerSpec[];
  storageRoot?: string;
};

export type BrowserUseSessionMcpSpecPreparerOptions = {
  env: Record<string, string | undefined>;
  buildArgs(
    baseArgs: string[],
    outputDir: string,
    env: Record<string, string | undefined>,
    configProxy?: PilotProxyConfig,
  ): string[];
  createDirectory?: (path: string) => void;
};

/**
 * Native preparation provider for the per-session browser-use MCP process.
 *
 * It owns neither the MCP runtime nor its registration/lease. It only turns
 * an already selected browser-use stdio spec into one exact session-local
 * process configuration before SessionMcpRuntimeBundle starts that runtime.
 */
export class BrowserUseSessionMcpSpecPreparer {
  private readonly createDirectory: (path: string) => void;

  constructor(private readonly options: BrowserUseSessionMcpSpecPreparerOptions) {
    this.createDirectory = options.createDirectory ?? ((path) => mkdirSync(path, { recursive: true }));
  }

  prepare(input: BrowserUseSessionMcpSpecPreparationInput): PilotDeckMcpServerSpec[] {
    return input.specs.map((spec) => {
      if (spec.transport !== "stdio" || spec.id !== "browser-use") return spec;
      const outputDir = input.storageRoot
        ? join(input.storageRoot, ".pilotdeck", "browser", "download")
        : join(input.projectRoot, ".pilotdeck", "browser_screenshots", sanitizeSessionIdForPath(input.sessionKey));
      this.createDirectory(outputDir);
      const profileDir = input.storageRoot ? join(input.storageRoot, ".pilotdeck", "browser", "profile") : undefined;
      if (profileDir) this.createDirectory(profileDir);
      const baseArgs = profileDir ? removePathArgs(spec.args ?? []) : spec.args ?? [];
      const args = this.options.buildArgs(baseArgs, outputDir, this.options.env, input.proxy);
      return {
        ...spec,
        cwd: outputDir,
        args: profileDir ? [...args, "--user-data-dir", profileDir] : args,
        ...(profileDir ? { env: { ...spec.env, PILOTDECK_SESSION_BROWSER_PROXY: "1" } } : {}),
      };
    });
  }
}

function removePathArgs(args: readonly string[]): string[] {
  const result: string[] = [];
  for (let i = 0; i < args.length; i++) {
    const arg = args[i]!;
    if (["--output-dir", "--user-data-dir"].includes(arg)) { i++; continue; }
    if (arg.startsWith("--output-dir=") || arg.startsWith("--user-data-dir=")) continue;
    result.push(arg);
  }
  return result;
}
