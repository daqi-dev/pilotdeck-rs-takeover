import {
  createMcpToolDefinitionsFromRuntime,
  createNativeMcpRuntime,
  type McpRuntimeFactory,
  type ProjectMcpRuntimeProvider,
} from "../mcp/index.js";
import type { PilotDeckMcpServerSpec } from "../mcp/protocol/types.js";
import type { ToolRegistry } from "../tool/index.js";
import { SessionMcpRuntimeRegistry } from "../mcp/runtime/SessionMcpRuntimeRegistry.js";
import { GatewaySessionResourceLeaseBundle } from "./GatewaySessionResourceLeaseBundle.js";
import { join } from "node:path";
import { McpRuntime } from "../mcp/runtime/McpRuntime.js";

export type SessionMcpRuntimeBundleOptions = {
  sessionKey: string;
  baseTools: ToolRegistry;
  mcpProvider: ProjectMcpRuntimeProvider;
  mcpServers: Record<string, unknown>;
  resources: GatewaySessionResourceLeaseBundle;
  perSessionRuntimes: SessionMcpRuntimeRegistry;
  maxPerSessionInstances: number;
  /** Host session root used to keep stdio MCP scratch state private. */
  sessionStorageRoot?: string;
  networkFetch?: typeof fetch;
  prepareSubprocess?: (request: {
    executable: string;
    args: readonly string[];
    cwd: string;
    env: NodeJS.ProcessEnv;
  }) => Promise<{ executable: string; args: readonly string[]; cwd: string; env: NodeJS.ProcessEnv }>;
  createRuntime?: McpRuntimeFactory;
  preparePerSessionSpecs?: (
    specs: readonly PilotDeckMcpServerSpec[],
    context?: { sessionKey: string; storageRoot?: string },
  ) => PilotDeckMcpServerSpec[];
  onDiagnostic?: (message: string, error?: unknown) => void;
};

/**
 * Composes MCP tools for one Agent session without becoming the owner of
 * project MCP state, plugin state, or Gateway session state. The supplied
 * resource bundle owns release order: per-session MCP -> shared MCP ->
 * plugin contribution -> project runtime.
 */
export class SessionMcpRuntimeBundle {
  constructor(private readonly options: SessionMcpRuntimeBundleOptions) {}

  async compose(): Promise<ToolRegistry> {
    const confined = Boolean(this.options.prepareSubprocess);
    const sharedLease = confined
      ? { tools: [], release: async () => {} }
      : await this.options.mcpProvider.acquireSharedRuntime(this.options.mcpServers);
    try {
      this.options.resources.add("shared MCP runtime lease", sharedLease.release);
    } catch (error) {
      await sharedLease.release().catch(() => undefined);
      throw error;
    }

    const tools = this.options.baseTools.clone();
    for (const definition of sharedLease.tools) {
      if (!tools.has(definition.name)) tools.register(definition);
    }

    const perSessionSpecs = this.options.mcpProvider.getPerSessionServerSpecs(
      this.options.mcpServers,
      confined,
    );
    if (!perSessionSpecs || perSessionSpecs.length === 0) return tools;
    if (this.options.perSessionRuntimes.size >= this.options.maxPerSessionInstances) {
      this.options.onDiagnostic?.(
        `Per-session MCP limit reached (${this.options.maxPerSessionInstances}). ` +
          `Session ${this.options.sessionKey} will not start: ${perSessionSpecs.map((spec) => spec.id).join(", ")}.`,
      );
      if (confined) throw new Error("Session MCP capacity exceeded");
      return tools;
    }

    const preparedSpecs = this.options.preparePerSessionSpecs
      ? this.options.preparePerSessionSpecs(perSessionSpecs, { sessionKey: this.options.sessionKey, storageRoot: this.options.sessionStorageRoot })
      : [...perSessionSpecs];
    const specs = await Promise.all(preparedSpecs.map(async (spec) => {
      if (confined && spec.transport !== "stdio" && !this.options.networkFetch) {
        throw new Error(`Session MCP ${spec.id} requires a controlled egress transport`);
      }
      if (!this.options.sessionStorageRoot || spec.transport !== "stdio" || !spec.perSession) return spec;
      const root = this.options.sessionStorageRoot;
      const sessionSpec = {
        ...spec,
        cwd: join(root, "workspace"),
        env: {
          ...spec.env,
          HOME: join(root, "home"),
          TMPDIR: join(root, "tmp"),
          XDG_CONFIG_HOME: join(root, "home", ".config"),
          XDG_CACHE_HOME: join(root, "home", ".cache"),
          PILOTDECK_SESSION_STORAGE_ROOT: root,
          PILOTDECK_SESSION_TMPDIR: join(root, "tmp", "mcp"),
        },
      };
      if (!this.options.prepareSubprocess) return sessionSpec;
      const prepared = await this.options.prepareSubprocess({
        executable: sessionSpec.command,
        args: sessionSpec.args ?? [],
        cwd: sessionSpec.cwd,
        env: { ...process.env, ...sessionSpec.env },
      });
      return {
        ...sessionSpec,
        command: prepared.executable,
        args: [...prepared.args],
        cwd: prepared.cwd,
        env: prepared.env as Record<string, string>,
      };
    }));
    const runtime = confined && this.options.networkFetch
      ? new McpRuntime(specs, { clientOptions: { fetch: this.options.networkFetch } })
      : (this.options.createRuntime ?? createNativeMcpRuntime)(specs);
    const registration = this.options.perSessionRuntimes.register(this.options.sessionKey, runtime);
    try {
      this.options.resources.add("per-session MCP runtime", registration.dispose);
    } catch (error) {
      await registration.dispose().catch(() => undefined);
      throw error;
    }

    try {
      const statuses = await runtime.start();
      for (const status of statuses) {
        if (status.status === "error") {
          this.options.onDiagnostic?.(
            `${status.serverId === "funasr" ? "ASR unavailable" : "Per-session MCP unavailable"} ` +
              `(server=${status.serverId}, session=${this.options.sessionKey}): ${status.error ?? "unknown error"}`,
          );
        }
      }
      const definitions = await createMcpToolDefinitionsFromRuntime(runtime);
      for (const definition of definitions) {
        if (tools.has(definition.name)) {
          tools.replace(definition);
        } else {
          tools.register(definition);
        }
      }
    } catch (error) {
      this.options.onDiagnostic?.(
        `Per-session MCP startup failed for ${this.options.sessionKey}`,
        error,
      );
    }
    return tools;
  }
}
