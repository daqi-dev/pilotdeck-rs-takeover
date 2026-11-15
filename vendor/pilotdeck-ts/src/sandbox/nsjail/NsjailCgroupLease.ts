import { access, mkdir, readFile, readdir, rmdir, stat, writeFile } from "node:fs/promises";
import { constants } from "node:fs";
import { join } from "node:path";
import type { SessionIsolationPolicy } from "../SessionExecutionProvider.js";
import { SessionExecutionProviderError } from "../SessionExecutionProvider.js";

/** One delegated cgroup parent accounts for all commands in a session generation. */
export async function acquireNsjailCgroupLease(root: string, sandboxKey: string, generation: number, policy: SessionIsolationPolicy) {
  await access(root, constants.W_OK | constants.X_OK);
  const controllers = (await readFile(join(root, "cgroup.controllers"), "utf8")).trim().split(/\s+/);
  if (!controllers.includes("memory") || !controllers.includes("pids")) {
    throw new SessionExecutionProviderError("Delegated cgroup requires memory and pids controllers", "provider_unavailable");
  }
  const prefix = `session-${sandboxKey}-`;
  for (const entry of await readdir(root, { withFileTypes: true })) {
    if (!entry.isDirectory() || !entry.name.startsWith(prefix)) continue;
    const previous = Number(entry.name.slice(prefix.length));
    if (!Number.isSafeInteger(previous) || previous >= generation) {
      throw new SessionExecutionProviderError("Conflicting cgroup generation", "session_conflict");
    }
    const stale = join(root, entry.name);
    await writeFile(join(stale, "cgroup.kill"), "1");
    await removeCgroup(stale);
  }
  const path = join(root, `session-${sandboxKey}-${generation}`);
  await mkdir(path);
  try {
    await writeFile(join(path, "cgroup.subtree_control"), "+memory +pids");
    if (policy.maxMemoryBytes) await writeFile(join(path, "memory.max"), String(policy.maxMemoryBytes));
    await writeFile(join(path, "pids.max"), String(policy.maxPids ?? 512));
  } catch (error) {
    await removeCgroup(path);
    throw error;
  }
  let releasePromise: Promise<void> | undefined;
  return {
    path,
    async kill() {
      try { await writeFile(join(path, "cgroup.kill"), "1"); }
      catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
    },
    release() {
      return releasePromise ??= removeCgroup(path);
    },
  };
}

/** Persistent storage identity separates providers sharing one delegated root. */
export async function nsjailCgroupSessionKey(storageRoot: string, sandboxKey: string): Promise<string> {
  const metadata = await stat(storageRoot, { bigint: true });
  return `${metadata.dev}-${metadata.ino}-${sandboxKey}`;
}

async function removeCgroup(path: string): Promise<void> {
  let children;
  try { children = await readdir(path, { withFileTypes: true }); }
  catch (error) { if ((error as NodeJS.ErrnoException).code === "ENOENT") return; throw error; }
  for (const child of children) if (child.isDirectory()) await removeCgroup(join(path, child.name));
  for (let attempt = 0; ; attempt++) {
    try { await rmdir(path); return; }
    catch (error) {
      const code = (error as NodeJS.ErrnoException).code;
      if (code === "ENOENT") return;
      if (code !== "EBUSY" || attempt >= 50) throw error;
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
  }
}
