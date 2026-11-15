import { AsyncLocalStorage } from "node:async_hooks";
import { isDeepStrictEqual } from "node:util";
import type { ExternalModuleBinding } from "./types.js";
import { HttpModuleClient } from "./HttpModuleClient.js";

export type SopAuthorityContext = Readonly<{
  sessionKey: string; projectKey: string; expectedRevision: number; requestId: string;
}>;
export type SopOptionalCapability = Readonly<{
  operation: "knowledge.search/v1"; resourceType: "knowledge_base"; resourceId: string;
  required: false; providerModuleId: "knowledge.local"; providerVersion: string;
  selectionMode: "current";
}>;
export type SopAuthorityProjection = Readonly<{
  context: SopAuthorityContext; sopId: string; sopVersion: string; nodeId: string;
  snapshotId: string; registryGeneration: number;
  optionalCapabilities: readonly SopOptionalCapability[];
}>;
export type SopCapabilityAuthorityPort = Readonly<{
  resolve(context: SopAuthorityContext, signal?: AbortSignal): Promise<SopAuthorityProjection>;
}>;

export type SopKnowledgeExecutionContext = SopAuthorityContext & Readonly<{ snapshotId: string; registryGeneration: number }>;
const knowledgeAuthority = new AsyncLocalStorage<SopKnowledgeExecutionContext>();
export const currentKnowledgeAuthority = () => knowledgeAuthority.getStore();
export function withKnowledgeAuthority<T>(context: SopKnowledgeExecutionContext, call: () => Promise<T>): Promise<T> {
  return knowledgeAuthority.run(context, call);
}

export const OPTIONAL_KNOWLEDGE_SCHEMA = {
  type: "object" as const, required: ["query"], additionalProperties: false,
  properties: {
    query: { type: "string" },
    knowledgeBaseIds: { type: "array", items: { type: "string" } },
    knowledgeBaseVersionIds: { type: "array", items: { type: "string" } },
    documentIds: { type: "array", items: { type: "string" } },
    maxChunks: { type: "integer", minimum: 1, maximum: 12 },
  },
};

export function validateOptionalKnowledgeInput(input: unknown): void {
  const value = input as Record<string, unknown>;
  if (!value || typeof value !== "object" || Array.isArray(value)
    || Object.keys(value).some(key => !Object.hasOwn(OPTIONAL_KNOWLEDGE_SCHEMA.properties, key))
    || typeof value.query !== "string" || !value.query.trim()
    || ["knowledgeBaseIds", "knowledgeBaseVersionIds", "documentIds"].some(key =>
      value[key] !== undefined && (!Array.isArray(value[key])
        || !(value[key] as unknown[]).every(id => typeof id === "string" && Boolean(id.trim()))))
    || (value.maxChunks !== undefined && (!Number.isInteger(value.maxChunks)
      || Number(value.maxChunks) < 1 || Number(value.maxChunks) > 12))) throw authorityError("SOP_AUTHORITY_INPUT_INVALID");
}

export function validateSopAuthorityProjection(value: unknown, context: SopAuthorityContext): SopAuthorityProjection {
  const v = value as SopAuthorityProjection;
  if (!v || !isDeepStrictEqual(v.context, context)
    || ![v.sopId, v.sopVersion, v.nodeId, v.snapshotId].every(s => typeof s === "string" && s.trim())
    || !Number.isInteger(v.registryGeneration) || v.registryGeneration < 0
    || !Array.isArray(v.optionalCapabilities) || !v.optionalCapabilities.every(c =>
      c && c.operation === "knowledge.search/v1" && c.resourceType === "knowledge_base"
      && typeof c.resourceId === "string" && c.resourceId.trim() && c.required === false
      && c.providerModuleId === "knowledge.local" && typeof c.providerVersion === "string"
      && c.providerVersion.trim() && c.selectionMode === "current")) throw authorityError("SOP_AUTHORITY_RESPONSE_INVALID");
  return v;
}

export function createSopCapabilityAuthorityPort(binding: ExternalModuleBinding): SopCapabilityAuthorityPort {
  const client = new HttpModuleClient({ ...binding,
    implementationId: "staffdeck.sop-capability-authority", contract: "staffdeck.sop-capability-authority/v1",
    manifestPath: "/api/v1/sop-capability-authority/module-manifest",
    callPath: `/api/v1/agents/${encodeURIComponent(binding.agentId!)}/sop-capability-authority/v2/module/call`,
    methods: ["resolve"],
  });
  return {
    async resolve(context, signal) {
      const response = await client.call({ runId: `sop:${context.sessionKey}`, operationId: "sop-authority.resolve",
        requestId: context.requestId, module: "capability", payload: { operation: "resolve", input: context }, abortSignal: signal });
      if (!response.ok) throw authorityError(typeof response.error?.code === "string" ? response.error.code : response.code ?? "SOP_AUTHORITY_UNAVAILABLE");
      return validateSopAuthorityProjection(response.payload?.result, context);
    },
  };
}

export function authorityError(code: string): Error & { code: string; status: number } {
  return Object.assign(new Error(code), { code, status: code.includes("REVISION") ? 409 : 403 });
}
