import { AgentSession, createAgentSessionStateFromReplay } from "../../agent/session/AgentSession.js";
import { TurnRunner, type AgentLoopRunner } from "../../agent/turn/TurnRunner.js";
import { createSubagentProjectSessionStorage, type AgentProjectSessionStorage } from "../../session/storage/ProjectSessionStorage.js";
import { replayTranscriptEntries } from "../../session/transcript/TranscriptReplay.js";
import { AgentSessionEventRecorder } from "../../agent/session/AgentSessionEventRecorder.js";
import { createDurableContextRuntime, unwrapDurableContextRuntime } from "../../agent/modules/context/durableContextRuntime.js";
import type { AgentContextRuntime } from "../../context/ContextRuntime.js";
import type { ModelInvokerPort, ToolPort } from "../../agent/modules/protocol.js";
import { createDurableModelInvokerPort } from "../../agent/modules/llm/durableModelInvokerPort.js";
import { createDurableToolPort } from "../../agent/modules/capability/durableToolPort.js";
import { createDurablePermissionAuditRecorder, unwrapDurablePermissionAuditRecorder } from "../../agent/modules/permission/durablePermissionAudit.js";
import type { PilotDeckToolAuditRecorder } from "../../tool/audit/ToolAuditRecorder.js";

export function sopNodeSessionId(parentSessionId: string, skillId: string, nodeId: string): string {
  return `${parentSessionId}:sop-node:${encodeURIComponent(skillId)}:${encodeURIComponent(nodeId)}`;
}

/** Each node owns an AgentSession, sequenced event runtime and durable transcript. */
export class SopNodeSessions {
  private readonly sessions = new Map<string, { session: AgentSession; storage: AgentProjectSessionStorage;
    context?: AgentContextRuntime; model?: ModelInvokerPort; tools?: ToolPort }>();

  constructor(private readonly options: {
    cwd: string;
    stateRoot: string;
    storage?: AgentProjectSessionStorage;
    runner: AgentLoopRunner;
    context?: AgentContextRuntime;
    ports?: { model: ModelInvokerPort; tools: ToolPort };
    auditRecorder?: PilotDeckToolAuditRecorder;
  }) {}

  async get(parentSessionId: string, skillId: string, nodeId: string) {
    const sessionId = sopNodeSessionId(parentSessionId, skillId, nodeId);
    const existing = this.sessions.get(sessionId);
    if (existing) return existing;
    const sidechainId = sopNodeSidechainId(skillId, nodeId);
    const storage = this.options.storage?.createSidechainStorage({ sessionId, subagentId: sidechainId })
      ?? createSubagentProjectSessionStorage({ projectRoot: this.options.cwd, pilotHome: this.options.stateRoot,
        parentSessionId, sessionId, sidechainId });
    try {
      const restored = await storage.restore();
      const replay = replayTranscriptEntries(restored.entries);
      if (!replay.metadata.parentSessionId) {
        await storage.transcript.recordSessionMetadata(sessionId, "sop-node-created", { parentSessionId });
      } else if (replay.metadata.parentSessionId !== parentSessionId) {
        throw new Error("SOP_NODE_PARENT_MISMATCH");
      }
      const eventRecorder = new AgentSessionEventRecorder(storage.transcript, { restoredEntries: restored.entries });
      const context = this.options.context ? createDurableContextRuntime(unwrapDurableContextRuntime(this.options.context), eventRecorder) : undefined;
      const model = this.options.ports ? createDurableModelInvokerPort(this.options.ports.model, eventRecorder, sessionId) : undefined;
      const audit = createDurablePermissionAuditRecorder(unwrapDurablePermissionAuditRecorder(this.options.auditRecorder),
        eventRecorder, { sessionId });
      const nodeAudit: PilotDeckToolAuditRecorder = {
        recordPermission: record => audit.recordPermission({ ...record, sessionId }),
        recordPermissionStarted: record => audit.recordPermissionStarted!({ ...record, sessionId }),
        recordPermissionFailed: record => audit.recordPermissionFailed!({ ...record, sessionId }),
        recordTool: record => audit.recordTool(record),
      };
      const tools = this.options.ports ? createDurableToolPort({
        list: () => this.options.ports!.tools.list(),
        ...(this.options.ports.tools.refresh ? { refresh: () => this.options.ports!.tools.refresh!() } : {}),
        executeAll: (calls, context, execution) => this.options.ports!.tools.executeAll(calls,
          { ...context, auditRecorder: nodeAudit }, execution),
      }, eventRecorder, sessionId) : undefined;
      const runner = new TurnRunner(this.options.runner, storage.transcript, undefined, undefined, undefined,
        { cwd: this.options.cwd, transcriptPath: storage.transcriptPath, collectFileArtifacts: false }, { eventRecorder });
      const session = new AgentSession({ sessionId, turnRunner: runner,
        cwd: this.options.cwd, transcriptPath: storage.transcriptPath,
        initialState: createAgentSessionStateFromReplay(sessionId, replay),
        replayEvents: replay.events, restoredEntries: restored.entries });
      const entry = { session, storage, context, model, tools };
      this.sessions.set(sessionId, entry);
      return entry;
    } catch (error) {
      await storage.dispose();
      throw error;
    }
  }

  async dispose(): Promise<void> {
    for (const { session, storage } of this.sessions.values()) {
      await session.dispose();
      await storage.dispose();
    }
    this.sessions.clear();
  }
}

export function sopNodeSidechainId(skillId: string, nodeId: string): string {
  return `sop-node-${encodeURIComponent(JSON.stringify([skillId, nodeId]))}`;
}
