import {
  TokenAccountingRuntime,
  type CompactionPort,
  type InstructionStoragePort,
  type MemoryResolver,
  type PromptCacheCoordinatorPort,
  type ToolResultSpillPort,
} from "../context/index.js";
import {
  PluginRuntime,
  type PilotDeckLoadedPlugin,
} from "../extension/index.js";
import {
  ProjectMcpRuntimeProvider,
  type McpRuntimeFactory,
} from "../mcp/index.js";
import {
  type ModelInvocationProviderRegistry,
  type ModelInvocationProvider,
  type ModelRuntime,
} from "../model/index.js";
import { loadPilotConfig, type PilotConfigSnapshot } from "../pilot/index.js";
import {
  createNativeRouterSessionCustomRouterPort,
  type RouterRuntime,
  type RouterProviderHealthPort,
  type RouterSessionCustomRouterPort,
  type RouterSessionStatePort,
} from "../router/index.js";
import {
  type ExecutionWorldBundle,
  type PilotDeckToolDefinition,
  type SandboxMode,
  type ToolRegistry,
} from "../tool/index.js";
import { join } from "node:path";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { SessionExecutionLease } from "../sandbox/SessionExecutionLease.js";
import {
  SessionExecutionProviderError,
  type SessionIsolationPolicy,
  type SessionExecutionProvider,
  type SessionExecutionHandle,
} from "../sandbox/SessionExecutionProvider.js";
import {
  createNativeSessionTitleProvider,
  type SessionTitlePort,
} from "../session/index.js";
import { LspService, type LspServicePort } from "../lsp/index.js";
import type { TelemetryClient } from "../telemetry/index.js";
import { ProjectModelRuntimeBundle } from "./ProjectModelRuntimeBundle.js";
import { ProjectExecutionWorldBundle } from "./ProjectExecutionWorldBundle.js";
import {
  ProjectContextStorageBundle,
  type ProjectContextStorageBundleOptions,
} from "./ProjectContextStorageBundle.js";
import {
  ProjectMemoryBundle,
  type ProjectMemoryProviderFactory,
  type ProjectMemoryManagementService,
  type ProjectMemoryMaintenanceService,
} from "./ProjectMemoryBundle.js";
import {
  resolvePilotDeckRuntimeProfile,
  type PilotDeckRuntimeProfile,
  type PilotDeckRuntimeProfileOverrides,
} from "./PilotDeckRuntimeProfile.js";
import {
  ProjectRouterRuntimeBundle,
  type ProjectRouterRuntimeBundleOptions,
} from "./ProjectRouterRuntimeBundle.js";

/**
 * The concrete resources needed by one project runtime generation. This is
 * deliberately application-scoped: it owns provider lifecycle, but not
 * Gateway sessions, project leases, or user-visible operation state.
 */
export type ProjectRuntimeResources = {
  snapshot: PilotConfigSnapshot;
  /** Immutable native provider selection for this published generation. */
  profile: PilotDeckRuntimeProfile;
  model: ModelRuntime;
  modelProviders: ModelInvocationProviderRegistry;
  tokenAccounting: TokenAccountingRuntime;
  router: RouterRuntime;
  routerSessionCustomRouters: RouterSessionCustomRouterPort;
  pluginRuntime: PluginRuntime;
  tools: ToolRegistry;
  /** Creates session-local tool closures for a session execution world. */
  createSessionTools: (executionWorld: ExecutionWorldBundle, lsp?: LspServicePort) => ToolRegistry;
  /** Optional session provider used to bind one execution world per session. */
  sessionExecutionProvider?: SessionExecutionProvider;
  createSessionExecution?: (input: {
    sessionKey: string;
    generation: number;
  }) => Promise<{ handle: SessionExecutionHandle; release: () => Promise<void> }>;
  mcpProvider: ProjectMcpRuntimeProvider;
  executionWorld: ExecutionWorldBundle;
  instructionStorage: InstructionStoragePort;
  toolResultSpill: ToolResultSpillPort;
  compaction?: CompactionPort;
  promptCacheCoordinator?: PromptCacheCoordinatorPort;
  sessionTitleProvider?: SessionTitlePort;
  lsp: LspServicePort;
  memory?: MemoryResolver;
  memoryService?: ProjectMemoryMaintenanceService;
  memoryManagement?: ProjectMemoryManagementService;
};

export type ProjectRuntimeResourcesBundleOptions = {
  projectRoot: string;
  pilotHome: string;
  builtinSkillsRoot?: string;
  env: Record<string, string | undefined>;
  now: () => Date;
  telemetry: TelemetryClient;
  extraTools: readonly PilotDeckToolDefinition[];
  subagentIdFactory?: () => string;
  builtinPlugins: readonly PilotDeckLoadedPlugin[];
  modelFactory?: (snapshot: PilotConfigSnapshot) => ModelRuntime;
  modelInvocationProviderFactory?: (snapshot: PilotConfigSnapshot) => readonly ModelInvocationProvider[];
  executionWorldBundleFactory?: (input: {
    projectRoot: string;
    now: () => Date;
    sandboxMode: SandboxMode;
  }) => ExecutionWorldBundle;
  /** Optional project-scoped provider for per-session execution worlds. */
  sessionExecutionProvider?: SessionExecutionProvider;
  /** Stable host root containing one directory per session sandbox key. */
  sessionExecutionStorageRoot?: string;
  /** Host-owned limits applied to every session execution binding. */
  sessionIsolationPolicy?: SessionIsolationPolicy;
  mcpRuntimeFactory?: McpRuntimeFactory;
  /** Application-selected context I/O providers for each published generation. */
  contextStorage?: ProjectContextStorageBundleOptions;
  /** Application-selected project memory provider for each published generation. */
  memoryProviderFactory?: ProjectMemoryProviderFactory;
  /** Application-selected compaction provider for each published generation. */
  compactionProviderFactory?: (input: {
    projectRoot: string;
    snapshot: PilotConfigSnapshot;
    now: () => Date;
  }) => CompactionPort | undefined;
  /** Application-selected prompt-cache generation provider for each generation. */
  promptCacheCoordinatorFactory?: (input: {
    projectRoot: string;
    snapshot: PilotConfigSnapshot;
    now: () => Date;
  }) => PromptCacheCoordinatorPort | undefined;
  /** Application-selected provider for session title generation. */
  sessionTitleProviderFactory?: (input: {
    projectRoot: string;
    snapshot: PilotConfigSnapshot;
    modelRuntime: ModelRuntime;
    now: () => Date;
  }) => SessionTitlePort | undefined;
  /** Application-selected project-generation LSP capability provider. */
  lspServiceFactory?: (input: { projectRoot: string; now: () => Date }) => LspServicePort;
  /** Application-selected per-generation session custom-router provider. */
  routerSessionCustomRouterFactory?: () => RouterSessionCustomRouterPort;
  /** Application-selected per-generation Router provider-health policy. */
  routerProviderHealthFactory?: (input: { now: () => number }) => RouterProviderHealthPort;
  modelPolicy?: {
    isAllowed(snapshot: PilotConfigSnapshot, model: { provider: string; model: string }): boolean;
    assertAllowed(snapshot: PilotConfigSnapshot, model: { provider: string; model: string }): void;
  };
  /** Application profile choices frozen with this project runtime generation. */
  runtimeProfileOverrides?: PilotDeckRuntimeProfileOverrides;
  createRouterConfig: ProjectRouterRuntimeBundleOptions["createRouterConfig"];
  createRouterEventBus: ProjectRouterRuntimeBundleOptions["createRouterEventBus"];
  /** Application-owned volatile routing state, retained across generation reloads. */
  routerSessionState?: RouterSessionStatePort;
  onDiagnostic?: (message: string, error?: unknown) => void;
};

/**
 * Native application provider for the resources that make up a project
 * runtime generation. It permits full staging before publication and owns
 * exactly the resources it constructs, including partial-build cleanup.
 */
export class ProjectRuntimeResourcesBundle {
  private resources: Partial<ProjectRuntimeResources> = {};
  private executionWorldBundle?: ProjectExecutionWorldBundle;
  private compaction?: CompactionPort;
  private sessionTitleProvider?: SessionTitlePort;
  private lspService?: LspServicePort;
  private memoryBundle?: ProjectMemoryBundle;
  private modelRuntime?: ProjectModelRuntimeBundle;
  private routerRuntime?: ProjectRouterRuntimeBundle;
  private readonly sessionGenerations = new Map<string, number>();
  private staged = false;
  private disposePromise?: Promise<void>;

  constructor(private readonly options: ProjectRuntimeResourcesBundleOptions) {}

  stage(): ProjectRuntimeResources {
    if (this.staged) {
      throw new Error("ProjectRuntimeResourcesBundle.stage called more than once.");
    }

    const snapshot = loadPilotConfig({
      projectRoot: this.options.projectRoot,
      env: this.options.env,
    });
    const profile = resolvePilotDeckRuntimeProfile({
      agent: snapshot.config.agent,
      ...this.options.runtimeProfileOverrides,
    });
    const modelRuntime = new ProjectModelRuntimeBundle({
      snapshot,
      modelFactory: this.options.modelFactory,
      modelInvocationProviderFactory: this.options.modelInvocationProviderFactory,
    });
    this.modelRuntime = modelRuntime;
    const { model, modelProviders } = modelRuntime.stage();
    this.resources.modelProviders = modelProviders;

    const pluginRuntime = new PluginRuntime({
      projectRoot: this.options.projectRoot,
      pilotHome: this.options.pilotHome,
      builtinSkillsRoot: this.options.builtinSkillsRoot,
      builtinPlugins: [...this.options.builtinPlugins],
      builtinPluginsEnabled: snapshot.config.extension.builtinPluginsEnabled,
    });
    this.resources.pluginRuntime = pluginRuntime;
    const routerSessionCustomRouters = this.options.routerSessionCustomRouterFactory?.()
      ?? createNativeRouterSessionCustomRouterPort();
    this.resources.routerSessionCustomRouters = routerSessionCustomRouters;

    const routerRuntime = new ProjectRouterRuntimeBundle({
      snapshot,
      model,
      modelProviders,
      useModelRuntime: this.options.modelFactory !== undefined,
      extensions: pluginRuntime,
      now: this.options.now,
      telemetry: this.options.telemetry,
      customRouterRegistry: routerSessionCustomRouters,
      createProviderHealth: this.options.routerProviderHealthFactory,
      ...(this.options.modelPolicy ? {
        isModelAllowed: (model) => this.options.modelPolicy!.isAllowed(snapshot, model),
        assertModelAllowed: (model) => this.options.modelPolicy!.assertAllowed(snapshot, model),
      } : {}),
      createRouterConfig: this.options.createRouterConfig,
      createRouterEventBus: this.options.createRouterEventBus,
      sessionState: this.options.routerSessionState,
    });
    this.routerRuntime = routerRuntime;
    const { tokenAccounting, router } = routerRuntime.stage();
    this.resources.router = router;

    const lspService = this.options.lspServiceFactory?.({
      projectRoot: this.options.projectRoot,
      now: this.options.now,
    }) ?? new LspService();
    this.lspService = lspService;

    const executionWorldBundle = new ProjectExecutionWorldBundle({
      projectRoot: this.options.projectRoot,
      snapshot,
      profile,
      now: this.options.now,
      extraTools: this.options.extraTools,
      subagentIdFactory: this.options.subagentIdFactory,
      skills: {
        loader: (name) => pluginRuntime.loadSkillPrompt(name),
        lister: () => pluginRuntime.getAllSkills(),
      },
      lsp: lspService,
      executionWorldBundleFactory: this.options.executionWorldBundleFactory,
    });
    this.executionWorldBundle = executionWorldBundle;
    const { executionWorld, tools } = executionWorldBundle.stage();
    this.resources.executionWorld = executionWorld;
    this.resources.createSessionTools = (sessionWorld) => executionWorldBundle.createTools(sessionWorld);
    if (this.options.sessionExecutionProvider && !this.options.sessionExecutionStorageRoot) {
      throw new SessionExecutionProviderError(
        "sessionExecutionStorageRoot is required when a session execution provider is configured",
        "provider_missing",
      );
    }
    if (this.options.sessionExecutionProvider && this.options.sessionExecutionStorageRoot) {
      const provider = this.options.sessionExecutionProvider;
      const storageRoot = this.options.sessionExecutionStorageRoot;
      this.resources.sessionExecutionProvider = provider;
      this.resources.createSessionExecution = async ({ sessionKey, generation }) => {
        const readiness = await provider.probe();
        if (!readiness.ready) {
          throw new SessionExecutionProviderError(
            readiness.reason ?? `Execution provider is not ready: ${readiness.providerId}`,
            "provider_unavailable",
          );
        }
        const sandboxKey = Buffer.from(sessionKey, "utf8").toString("base64url");
        const nextGeneration = Math.max(
          generation,
          (this.sessionGenerations.get(sessionKey) ?? 0) + 1,
        );
        const sessionRoot = join(storageRoot, sandboxKey);
        const generationFile = join(sessionRoot, ".pilotdeck", "control", "generation");
        let persistedGeneration = 0;
        try {
          persistedGeneration = Number.parseInt(await readFile(generationFile, "utf8"), 10) || 0;
        } catch {
          // A new session has no durable generation marker yet.
        }
        const allocatedGeneration = Math.max(nextGeneration, persistedGeneration + 1);
        this.sessionGenerations.set(sessionKey, allocatedGeneration);
        const lease = new SessionExecutionLease(provider, {
          sessionKey,
          sandboxKey,
          generation: allocatedGeneration,
          storage: {
            workspace: join(sessionRoot, "workspace"),
            home: join(sessionRoot, "home"),
            temp: join(sessionRoot, "tmp"),
            pipCache: join(sessionRoot, "home", ".cache", "pip"),
            npmCache: join(sessionRoot, "home", ".cache", "npm"),
            control: join(sessionRoot, ".pilotdeck", "control"),
            spill: join(sessionRoot, ".pilotdeck", "spill"),
            artifact: join(sessionRoot, ".pilotdeck", "artifact"),
            browserProfile: join(sessionRoot, ".pilotdeck", "browser", "profile"),
            browserDownload: join(sessionRoot, ".pilotdeck", "browser", "download"),
          },
          policy: this.options.sessionIsolationPolicy ?? { network: "deny" },
        });
        const handle = await lease.acquire();
        await mkdir(join(sessionRoot, ".pilotdeck", "control"), { recursive: true });
        await writeFile(generationFile, `${allocatedGeneration}\n`, "utf8");
        return { handle, release: () => lease.release() };
      };
    }

    const contextStorage = new ProjectContextStorageBundle(this.options.contextStorage).stage();
    const compaction = this.options.compactionProviderFactory?.({
      projectRoot: this.options.projectRoot,
      snapshot,
      now: this.options.now,
    });
    this.compaction = compaction;
    const promptCacheCoordinator = this.options.promptCacheCoordinatorFactory?.({
      projectRoot: this.options.projectRoot,
      snapshot,
      now: this.options.now,
    });
    const sessionTitleProvider = this.options.sessionTitleProviderFactory?.({
      projectRoot: this.options.projectRoot,
      snapshot,
      modelRuntime: model,
      now: this.options.now,
    }) ?? createNativeSessionTitleProvider({
      modelRuntime: model,
      agentModel: snapshot.config.agent.model,
    });
    this.sessionTitleProvider = sessionTitleProvider;

    const mcpProvider = new ProjectMcpRuntimeProvider({
      projectRoot: this.options.projectRoot,
      pilotHome: this.options.pilotHome,
      createRuntime: this.options.mcpRuntimeFactory,
      onDiagnostic: (message, error) => {
        this.options.onDiagnostic?.(message, error);
      },
    });
    this.resources = {
      snapshot,
      profile,
      model,
      modelProviders,
      tokenAccounting,
      router,
      routerSessionCustomRouters,
      pluginRuntime,
      tools,
      createSessionTools: (sessionWorld, lsp) => executionWorldBundle.createTools(sessionWorld, lsp),
      mcpProvider,
      executionWorld,
      ...(this.resources.sessionExecutionProvider ? { sessionExecutionProvider: this.resources.sessionExecutionProvider } : {}),
      ...(this.resources.createSessionExecution ? { createSessionExecution: this.resources.createSessionExecution } : {}),
      ...contextStorage,
      ...(compaction ? { compaction } : {}),
      ...(promptCacheCoordinator ? { promptCacheCoordinator } : {}),
      sessionTitleProvider,
      lsp: lspService,
    };

    const memoryBundle = new ProjectMemoryBundle({
      config: snapshot.config.memory,
      modelConfig: snapshot.config.model,
      agentModel: snapshot.config.agent.model.id,
      projectRoot: this.options.projectRoot,
      now: this.options.now,
      telemetry: this.options.telemetry,
      providerFactory: this.options.memoryProviderFactory,
    });
    this.memoryBundle = memoryBundle;
    Object.assign(this.resources, memoryBundle.stage());
    this.staged = true;
    return this.resources as ProjectRuntimeResources;
  }

  async dispose(): Promise<void> {
    if (this.disposePromise) return this.disposePromise;
    this.disposePromise = this.disposeResources();
    return this.disposePromise;
  }

  private async disposeResources(): Promise<void> {
    const failures: unknown[] = [];
    try {
      await this.resources.mcpProvider?.dispose();
    } catch (error) {
      failures.push(error);
    }

    const results = await Promise.allSettled([
      this.resources.pluginRuntime?.dispose() ?? Promise.resolve(),
      Promise.resolve(this.resources.routerSessionCustomRouters?.dispose()),
      Promise.resolve((this.resources.compaction ?? this.compaction)?.dispose?.()),
      Promise.resolve((this.resources.sessionTitleProvider ?? this.sessionTitleProvider)?.dispose?.()),
      Promise.resolve((this.resources.lsp ?? this.lspService)?.dispose()),
      this.executionWorldBundle?.dispose() ?? Promise.resolve(),
      this.routerRuntime?.dispose() ?? Promise.resolve(),
      this.modelRuntime?.dispose() ?? Promise.resolve(),
    ]);
    failures.push(...results
      .filter((result): result is PromiseRejectedResult => result.status === "rejected")
      .map((result) => result.reason));
    try {
      await this.memoryBundle?.dispose();
    } catch (error) {
      failures.push(error);
    }

    if (failures.length === 1) throw failures[0];
    if (failures.length > 1) {
      throw new AggregateError(failures, "Failed to dispose project runtime resources.");
    }
  }
}
