/** DSH-style LSP capability seam. The protocol wire is intentionally hidden here. */
import type { FsPort } from "../../tool/execution-world/FsPort.js";
import type { SandboxedCommand } from "../../tool/execution-world/SandboxPort.js";

export type LspSessionExecution = {
  workspaceRoot: string;
  fs: FsPort;
  prepareSubprocess(request: SandboxedCommand): Promise<SandboxedCommand>;
};

export type LspOperation = "goToDefinition" | "findReferences" | "goToImplementation" | "hover";

export type LspPosition = { readonly line: number; readonly character: number };
export type LspRange = { readonly start: LspPosition; readonly end: LspPosition };

export type LspQueryRequest = {
  readonly operation: LspOperation;
  readonly filePath: string;
  readonly position: LspPosition;
  readonly workspaceRoot: string;
};

export type LspProviderQuery = LspQueryRequest & { readonly languageId: string };

export type LspLocation = { readonly uri: string; readonly range: LspRange };
export type LspHover = { readonly contents: string; readonly range?: LspRange };

export type LspQueryResult =
  | { readonly kind: "locations"; readonly locations: readonly LspLocation[]; readonly resolvedWorkspaceUri: string }
  | { readonly kind: "hover"; readonly hover: LspHover | null };

export type LspProvider = {
  readonly id: string;
  readonly extensionToLanguage: Readonly<Record<string, string>>;
  query(request: LspProviderQuery, signal?: AbortSignal): Promise<LspQueryResult>;
  dispose?(): void | Promise<void>;
  bindSession?(execution: LspSessionExecution): LspProvider;
};

export type LspService = {
  registerProvider(provider: LspProvider): () => Promise<void>;
  query(request: LspQueryRequest, signal?: AbortSignal): Promise<LspQueryResult>;
  dispose(): Promise<void>;
  bindSession?(execution: LspSessionExecution): LspService;
};
