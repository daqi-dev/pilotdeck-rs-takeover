/** Stateless text extraction for the selected PD file Port. Domain ingest owns its job. */
import { extname } from "node:path";

export type ActiveTextParseInput = {
  filename: string;
  mediaType?: unknown;
  bytes: Uint8Array;
  maxBytes?: unknown;
  signal?: AbortSignal;
};

function parseError(status: number, code: string, message: string) {
  return Object.assign(new Error(message), { status, code });
}

export function createActiveRuntimeTextParsingPort() {
  return async (input: ActiveTextParseInput) => {
    if (input.signal?.aborted) throw parseError(499, "PUBLIC_HOST_CANCELLED", "File extraction aborted");
    const extension = extname(input.filename).toLowerCase();
    if (![".txt", ".md", ".markdown", ".pdf"].includes(extension)) {
      throw parseError(415, "PUBLIC_FILE_TYPE_UNSUPPORTED", "This PD text extractor accepts PDF, .txt and Markdown");
    }
    const declared = input.mediaType;
    if (declared !== undefined && declared !== null && typeof declared !== "string") {
      throw parseError(400, "PUBLIC_FILE_INPUT_INVALID", "mediaType must be a string");
    }
    const limit = input.maxBytes === undefined ? 20 * 1024 * 1024 : input.maxBytes;
    if (!Number.isSafeInteger(limit) || Number(limit) <= 0 || Number(limit) > 20 * 1024 * 1024) {
      throw parseError(400, "PUBLIC_FILE_INPUT_INVALID", "maxBytes must be within 20 MiB");
    }
    if (input.bytes.byteLength > Number(limit)) {
      throw parseError(413, "PUBLIC_FILE_TOO_LARGE", "File exceeds selected extraction limit");
    }
    let text: string;
    if (extension === ".pdf") {
      try {
        const { Document } = await import("mupdf");
        const document = Document.openDocument(input.bytes, "application/pdf");
        try {
          const pages: string[] = [];
          for (let index = 0; index < document.countPages(); index++) {
            if (input.signal?.aborted) throw parseError(499, "PUBLIC_HOST_CANCELLED", "File extraction aborted");
            const page = document.loadPage(index);
            try {
              const content = page.toStructuredText();
              try { pages.push(content.asText()); } finally { content.destroy(); }
            } finally { page.destroy(); }
          }
          text = pages.join("\n");
        } finally { document.destroy(); }
      } catch (error) {
        if (input.signal?.aborted) throw parseError(499, "PUBLIC_HOST_CANCELLED", "File extraction aborted");
        throw parseError(422, "PUBLIC_FILE_PARSE_FAILED", "PDF text extraction failed");
      }
    } else {
      try {
        text = new TextDecoder("utf-8", { fatal: true }).decode(input.bytes);
      } catch {
        throw parseError(422, "PUBLIC_FILE_ENCODING_UNSUPPORTED", "File is not valid UTF-8");
      }
    }
    if (input.signal?.aborted) throw parseError(499, "PUBLIC_HOST_CANCELLED", "File extraction aborted");
    return { filename: input.filename, text, mediaType: declared ?? (extension === ".pdf" ? "application/pdf" : "text/plain"),
      metadata: { fileType: extension.slice(1), bytes: input.bytes.byteLength } };
  };
}
