import type { SessionExecutionHandle, TrustedSessionBinding } from "../../src/sandbox/SessionExecutionProvider.js";

export type ExecutionObservation = {
  sessionKey: string; sandboxKey: string; generation: number; port: string; method: string;
  request: unknown; response?: unknown; error?: string; startedAt: string; finishedAt?: string;
};

/** Observe the real ports without changing their synchronous or async contracts. */
export function observeExecution(handle: SessionExecutionHandle, binding: TrustedSessionBinding, observations: ExecutionObservation[]): SessionExecutionHandle {
  const observedPorts = new Set(["fs", "subprocess", "shell", "detachedShell", "codeRuntime", "attachmentDelivery", "planStorage", "backgroundTasks", "network"]);
  const world = new Proxy(handle.world, {
    get(target, port, receiver) {
      const value = Reflect.get(target, port, receiver);
      if (!observedPorts.has(String(port)) || !value) return value;
      return new Proxy(value, {
        get(object, method) {
          const member = Reflect.get(object, method, object);
          if (typeof member !== "function") return member;
          return (...args: unknown[]) => {
            const observation: ExecutionObservation = { sessionKey: binding.sessionKey, sandboxKey: binding.sandboxKey, generation: binding.generation,
              port: String(port), method: String(method), request: safeRequest(args), startedAt: new Date().toISOString() };
            observations.push(observation);
            const success = (result: unknown) => { observation.response = result; observation.finishedAt = new Date().toISOString(); return result; };
            const failure = (error: unknown): never => { observation.error = String(error); observation.finishedAt = new Date().toISOString(); throw error; };
            try {
              const result = member.apply(object, args);
              return result && typeof result.then === "function" ? result.then(success, failure) : success(result);
            } catch (error) { return failure(error); }
          };
        },
      });
    },
  });
  return { ...handle, world };
}

function safeRequest(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(safeRequest);
  if (!value || typeof value !== "object") return typeof value === "function" ? undefined : value;
  return Object.fromEntries(Object.entries(value).filter(([key]) => key !== "signal").map(([key, item]) => [key, key === "env" && item
    ? Object.fromEntries(Object.entries(item).filter(([name]) => /^(PATH|HOME|TMPDIR|PYTHONUSERBASE|PIP_CACHE_DIR|NPM_CONFIG_PREFIX|NPM_CONFIG_CACHE|NPM_CONFIG_USERCONFIG|LANG|LC_ALL|SESSION_ENV|SESSION_ONLY_VAR)$/.test(name)))
    : safeRequest(item)]));
}

export function attachCaseEvidence(cases: Array<Record<string, unknown>>, observations: ExecutionObservation[]) {
  const append = cases.push.bind(cases);
  let cursor = 0;
  let lastTrace: ExecutionObservation[] = [];
  cases.push = (...entries) => {
    const newTrace = observations.slice(cursor);
    if (newTrace.length) lastTrace = newTrace;
    const trace = newTrace.length ? newTrace : lastTrace;
    cursor = observations.length;
    const owners = [...new Map(trace.map((item) => [item.sandboxKey + item.generation, {
      sessionKey: item.sessionKey, sandboxKey: item.sandboxKey, generation: item.generation,
    }])).values()];
    return append(...entries.map((entry) => ({
      startedAt: trace[0]?.startedAt ?? new Date().toISOString(), finishedAt: new Date().toISOString(),
      evidenceScope: newTrace.length ? "operation-group" : trace.length ? "shared-operation-group" : "global",
      sessionKey: owners.map((owner) => owner.sessionKey), sandboxKey: owners.map((owner) => owner.sandboxKey), generation: owners.map((owner) => owner.generation),
      request: trace.map((item) => ({ port: item.port, method: item.method, arguments: item.request })),
      response: trace.map((item) => ({ result: item.response, error: item.error, startedAt: item.startedAt, finishedAt: item.finishedAt })),
      exitCode: entry.status === "PASS" || entry.status === "N/A" ? 0 : null, failureReason: null,
      ...entry,
    })));
  };
}
