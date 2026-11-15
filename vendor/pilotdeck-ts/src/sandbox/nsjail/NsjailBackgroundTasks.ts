import type { BackgroundTaskRuntime, StartTaskSpec } from "../../task/runtime/BackgroundTaskRuntime.js";
import type { BackgroundTaskCompletionHandler } from "../../task/runtime/BackgroundTaskCompletionEvents.js";
import type { TrustedSessionBinding } from "../SessionExecutionProvider.js";

/** The port instance is the trusted owner; tool identities never select it. */
export function bindNsjailBackgroundTasks(runtime: BackgroundTaskRuntime, binding: TrustedSessionBinding, assertActive: () => void): BackgroundTaskRuntime {
  const owner = Object.freeze({ sessionKey: binding.sessionKey, sandboxKey: binding.sandboxKey, generation: binding.generation });
  const owners = new Map<string, typeof owner>();
  const access = { sessionId: binding.sessionKey };
  const owns = (id: string) => owners.get(id) === owner;
  let pendingStarts = 0;
  const deferredCompletions: Array<() => void> = [];
  const methods = {
    async start(spec: StartTaskSpec) {
      assertActive();
      pendingStarts++;
      try {
        const task = await runtime.start({ ...spec, sessionId: binding.sessionKey });
        owners.set(task.taskId, owner);
        assertActive();
        return task;
      } finally {
        if (--pendingStarts === 0) for (const deliver of deferredCompletions.splice(0)) deliver();
      }
    },
    list(filter?: Parameters<BackgroundTaskRuntime["list"]>[0]) {
      assertActive();
      return runtime.list(filter, access).filter((task) => owns(task.taskId));
    },
    get(id: string) { assertActive(); return owns(id) ? runtime.get(id, access) : undefined; },
    getOutput(id: string, offset: number, maxBytes?: number) {
      assertActive();
      if (!owns(id)) throw new Error(`Unknown taskId: ${id}`);
      return runtime.getOutput(id, offset, maxBytes, access);
    },
    async wait(id: string, options?: Parameters<BackgroundTaskRuntime["wait"]>[1]) {
      assertActive();
      if (!owns(id)) return undefined;
      const result = await runtime.wait(id, options, access);
      assertActive();
      return result;
    },
    async stop(id: string, options?: Parameters<BackgroundTaskRuntime["stop"]>[1]) {
      assertActive();
      if (!owns(id)) throw new Error(`Unknown taskId: ${id}`);
      await runtime.stop(id, options, access);
    },
    subscribeCompletionEvents(handler: BackgroundTaskCompletionHandler) {
      assertActive();
      return runtime.subscribeCompletionEvents((event) => {
        const deliver = () => {
          try { assertActive(); } catch { return; }
          if (owns(event.taskId)) handler(event);
        };
        if (!owns(event.taskId) && pendingStarts > 0) deferredCompletions.push(deliver);
        else deliver();
      });
    },
  };
  return new Proxy(runtime, {
    get(target, property) {
      if (property in methods) return methods[property as keyof typeof methods];
      const value = Reflect.get(target, property, target);
      return typeof value === "function" ? value.bind(target) : value;
    },
  });
}
