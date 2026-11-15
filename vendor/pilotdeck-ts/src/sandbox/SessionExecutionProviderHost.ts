import {
  SessionExecutionProviderError,
  type SessionExecutionHandle,
  type SessionExecutionProvider,
  type SessionIsolationPolicy,
  type TrustedSessionBinding,
} from "./SessionExecutionProvider.js";

/** Host configuration updates affect future leases; existing handles retain their provider. */
export class SessionExecutionProviderHost implements SessionExecutionProvider {
  readonly id = "session-execution-host";
  readonly contractVersion = 1 as const;
  private current: { provider: SessionExecutionProvider; policy: SessionIsolationPolicy };
  private readonly providers = new Set<SessionExecutionProvider>();
  private readonly owners = new Map<string, SessionExecutionProvider>();
  private readonly active = new Set<string>();
  private readonly pending = new Set<Promise<SessionExecutionHandle>>();
  private disposed = false;
  private disposePromise?: Promise<void>;

  constructor(provider: SessionExecutionProvider, policy: SessionIsolationPolicy) {
    this.current = { provider, policy: { ...policy } };
    this.providers.add(provider);
  }

  replace(provider: SessionExecutionProvider, policy: SessionIsolationPolicy): void {
    this.assertActive();
    this.current = { provider, policy: { ...policy } };
    this.providers.add(provider);
  }

  probe() { this.assertActive(); return this.current.provider.probe(); }

  createSession(binding: TrustedSessionBinding): Promise<SessionExecutionHandle> {
    this.assertActive();
    if (this.active.has(binding.sandboxKey)) return Promise.reject(new SessionExecutionProviderError("Session already active", "session_conflict"));
    this.active.add(binding.sandboxKey);
    const { provider, policy } = this.current;
    const snapshot = { ...binding, storage: { ...binding.storage }, policy: { ...policy } };
    const key = snapshot.sandboxKey;
    const pending = Promise.resolve().then(() => provider.createSession(snapshot)).then((handle) => {
      this.owners.set(key, provider);
      let released: Promise<void> | undefined;
      return { ...handle, dispose: () => released ??= Promise.resolve().then(() => handle.dispose()).finally(() => this.active.delete(key)) };
    }, (error) => { this.active.delete(key); throw error; });
    this.pending.add(pending);
    void pending.then(() => this.pending.delete(pending), () => this.pending.delete(pending));
    return pending;
  }

  async forkSession(input: { source: TrustedSessionBinding; target: TrustedSessionBinding }) {
    this.assertActive();
    const provider = this.owners.get(input.source.sandboxKey) ?? this.current.provider;
    if (!provider.forkSession) throw new SessionExecutionProviderError("Provider does not support execution-storage fork", "provider_unavailable");
    return provider.forkSession(input);
  }

  async deleteSessionStorage(sessionKey: string): Promise<void> {
    this.assertActive();
    const key = Buffer.from(sessionKey).toString("base64url");
    if (this.active.has(key)) throw new SessionExecutionProviderError("Cannot delete active session storage", "session_conflict");
    const provider = this.owners.get(key) ?? this.current.provider;
    if (!provider.deleteSessionStorage) throw new SessionExecutionProviderError("Provider does not support storage deletion", "provider_unavailable");
    await provider.deleteSessionStorage(sessionKey);
    this.owners.delete(key);
  }

  dispose(): Promise<void> {
    if (this.disposePromise) return this.disposePromise;
    this.disposed = true;
    return this.disposePromise = (async () => {
      await Promise.allSettled([...this.pending]);
      const results = await Promise.allSettled([...this.providers].map((provider) => provider.dispose()));
      const errors = results.flatMap((result) => result.status === "rejected" ? [result.reason] : []);
      if (errors.length) throw new AggregateError(errors, "Session providers failed to dispose");
      this.active.clear();
    })();
  }

  private assertActive() {
    if (this.disposed) throw new SessionExecutionProviderError("Session provider host is disposed", "provider_unavailable");
  }
}
