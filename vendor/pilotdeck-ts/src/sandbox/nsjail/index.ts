import {
  NsjailSessionExecutionProvider,
  type NsjailProviderOptions,
} from "./NsjailProvider.js";

/**
 * Optional nsjail integration for hosts that explicitly opt into
 * per-session execution isolation. The core Gateway only consumes the
 * generic SessionExecutionProvider contract and never imports this module.
 */
export type NsjailSandboxModule = Readonly<{
  provider: NsjailSessionExecutionProvider;
  sessionExecutionStorageRoot: string;
  dispose(): Promise<void>;
}>;

export function createNsjailSandboxModule(options: NsjailProviderOptions): NsjailSandboxModule {
  const provider = new NsjailSessionExecutionProvider(options);
  return Object.freeze({
    provider,
    sessionExecutionStorageRoot: options.sessionsRoot,
    dispose: () => provider.dispose(),
  });
}

export {
  NsjailSessionExecutionProvider,
  type NsjailProviderOptions,
} from "./NsjailProvider.js";
