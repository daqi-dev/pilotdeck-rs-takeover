import type { PublicHostPrincipal } from "./nativeHostCapabilityProvider.js";
import type { SessionMetadataValue } from "../session/transcript/TranscriptEntry.js";
import { authorityError, type SopAuthorityContext } from "./sopCapabilityAuthority.js";

/** Read original host authority; transport identity alone is insufficient. */
export function createNativeSopAuthorityReader(options: {
  readOwner(): string | undefined;
  listProjects(): Promise<string[]>;
  listSessions(projectKey: string): Promise<readonly { sessionId: string }[]>;
  sessionAdmission(projectKey: string, sessionKey: string): Promise<SessionMetadataValue["staffDeckAdmission"]>;
  readState(context: SopAuthorityContext): Promise<{ sopId: string; sopVersion: string; nodeId: string; content: Record<string, unknown> }>;
}) {
  return async (input: SopAuthorityContext & { admissionCredentialId: string }, principal: PublicHostPrincipal) => {
    if (!input || Object.keys(input).some(key => !["sessionKey", "projectKey", "expectedRevision", "requestId", "admissionCredentialId"].includes(key))
      || ![input.sessionKey, input.projectKey, input.requestId, input.admissionCredentialId].every(v => typeof v === "string" && v.trim())
      || !Number.isInteger(input.expectedRevision) || input.expectedRevision < 1) throw authorityError("SOP_AUTHORITY_INPUT_INVALID");
    if (options.readOwner() !== principal.pilotDeckUserId
      || (await options.listProjects()).filter(key => key === input.projectKey).length !== 1
      || !(await options.listSessions(input.projectKey)).some(s => s.sessionId === input.sessionKey)) throw authorityError("SOP_AUTHORITY_SESSION_DENIED");
    const admission = await options.sessionAdmission(input.projectKey, input.sessionKey);
    if (!admission || admission.projectKey !== input.projectKey || admission.pilotDeckUserId !== principal.pilotDeckUserId
      || admission.tenantId !== principal.tenantId || admission.actorUserId !== principal.actorUserId
      || admission.agentId !== principal.agentId || admission.credentialId !== input.admissionCredentialId) throw authorityError("SOP_AUTHORITY_ADMISSION_MISMATCH");
    const { admissionCredentialId: _credential, ...context } = input;
    try {
      return { ...context, admission, ...await options.readState(context) };
    } catch (error) {
      const code = (error as { code?: unknown })?.code;
      // These owner-read refusals are expected public outcomes, not upstream failures.
      if (code === "SOP_REVISION_CONFLICT" || code === "SOP_AUTHORITY_STATE_INACTIVE") throw authorityError(code);
      throw error;
    }
  };
}
