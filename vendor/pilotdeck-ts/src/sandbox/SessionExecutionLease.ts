import {
  SessionExecutionProviderError,
  type SessionExecutionHandle,
  type SessionExecutionProvider,
  type TrustedSessionBinding,
} from "./SessionExecutionProvider.js";

/** Serializes creation and gives one session a single provider handle. */
export class SessionExecutionLease {
  private handle?: SessionExecutionHandle;
  private acquirePromise?: Promise<SessionExecutionHandle>;
  private released = false;

  constructor(
    private readonly provider: SessionExecutionProvider,
    private readonly binding: TrustedSessionBinding,
  ) {}

  acquire(): Promise<SessionExecutionHandle> {
    if (this.released) return Promise.reject(this.closedError());
    if (this.handle) return Promise.resolve(this.handle);
    if (!this.acquirePromise) {
      this.acquirePromise = this.provider.createSession(this.binding).then((handle) => {
        if (this.released) {
          return handle.dispose().then(() => Promise.reject(this.closedError()));
        }
        this.handle = handle;
        return handle;
      }).finally(() => { this.acquirePromise = undefined; });
    }
    return this.acquirePromise;
  }

  async stop(reason: string): Promise<void> {
    const handle = this.handle;
    if (handle) await handle.stop(reason);
  }

  async release(): Promise<void> {
    if (this.released) return;
    this.released = true;
    const handle = this.handle;
    this.handle = undefined;
    if (handle) await handle.dispose();
  }

  private closedError(): SessionExecutionProviderError {
    return new SessionExecutionProviderError(`Session execution lease is closed: ${this.binding.sessionKey}`, "session_closed");
  }
}
