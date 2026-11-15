import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { isAbsolute, relative, resolve } from "node:path";
import type { SessionIsolationPolicy, TrustedSessionBinding } from "./SessionExecutionProvider.js";
import type { AgentTranscriptEntry } from "../session/transcript/TranscriptEntry.js";
import type { CanonicalMessage } from "../model/protocol/canonical.js";
import { readCompactSnapshot } from "../session/transcript/CompactSnapshot.js";

/** Host-only layout shared by session creation and execution-storage fork. */
export async function nextSessionExecutionBinding(root: string, sessionKey: string, policy: SessionIsolationPolicy): Promise<TrustedSessionBinding> {
  const sandboxKey = Buffer.from(sessionKey).toString("base64url");
  const sessionRoot = join(root, sandboxKey);
  const control = join(sessionRoot, ".pilotdeck", "control");
  let generation = 1;
  try {
    const previous = Number((await readFile(join(control, "generation"), "utf8")).trim());
    if (!Number.isSafeInteger(previous) || previous < 1) throw new Error("Invalid persisted session generation");
    generation = previous + 1;
  } catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
  return {
    sessionKey, sandboxKey, generation, policy,
    storage: {
      workspace: join(sessionRoot, "workspace"), home: join(sessionRoot, "home"), temp: join(sessionRoot, "tmp"),
      pipCache: join(sessionRoot, "home", ".cache", "pip"), npmCache: join(sessionRoot, "home", ".cache", "npm"), control,
      spill: join(sessionRoot, ".pilotdeck", "spill"), artifact: join(sessionRoot, ".pilotdeck", "artifact"),
      browserProfile: join(sessionRoot, ".pilotdeck", "browser", "profile"), browserDownload: join(sessionRoot, ".pilotdeck", "browser", "download"),
    },
  };
}

export function retargetExecutionSnapshot(entry: AgentTranscriptEntry, root: string, sourceKey: string, targetKey: string): AgentTranscriptEntry {
  const source = join(root, Buffer.from(sourceKey).toString("base64url"));
  const target = join(root, Buffer.from(targetKey).toString("base64url"));
  const retarget = (path: string) => {
    const child = relative(source, resolve(path));
    if (child === ".." || child.startsWith("../") || isAbsolute(child)) throw new Error("Fork snapshot path is outside its source session");
    return join(target, child);
  };
  const retargetMessage = (message: CanonicalMessage): CanonicalMessage => ({
    ...message,
    content: message.content.map((block) => {
      if (block.type !== "tool_result_reference" && block.type !== "media_reference") return block;
      return { ...block, path: retarget(block.path) };
    }),
  });
  if (entry.type === "file_snapshot_recorded") return {
      ...entry,
      trackedFileBackups: Object.fromEntries(Object.entries(entry.trackedFileBackups).map(([path, value]) => [retarget(path), value])),
      ...(entry.expectedFileStates ? { expectedFileStates: Object.fromEntries(Object.entries(entry.expectedFileStates).map(([path, value]) => [retarget(path), value])) } : {}),
    };
  if (entry.type === "accepted_input") return { ...entry, messages: entry.messages.map(retargetMessage) };
  if (entry.type === "assistant_message" || entry.type === "tool_result_message" || entry.type === "durable_message") {
    return { ...entry, message: retargetMessage(entry.message) };
  }
  if (entry.type === "control_boundary" && entry.boundary.kind === "compact" && entry.boundary.subtype === "compact_boundary") {
    const snapshot = readCompactSnapshot(entry);
    if (snapshot) return { ...entry, boundary: { ...entry.boundary, snapshot: { version: 1, messages: snapshot.map(retargetMessage) } } };
  }
  return entry;
}
