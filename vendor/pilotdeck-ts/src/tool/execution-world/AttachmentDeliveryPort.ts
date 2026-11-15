import { realpath, stat } from "node:fs/promises";

/** Metadata required to deliver an already-authorized local attachment. */
export type AttachmentDeliveryMetadata = {
  size: number;
  kind: "file" | "directory" | "other";
};

/**
 * DSH-style attachment-delivery Definition.
 *
 * It intentionally has no workspace policy, permission decision, MIME
 * presentation, or channel transport. Those remain with the send_attachment
 * consumer and its host. The provider only resolves symlinks and inspects the
 * selected artifact after policy has admitted its path.
 */
export type AttachmentDeliveryPort = {
  realpath(path: string): Promise<string>;
  stat(path: string): Promise<AttachmentDeliveryMetadata>;
  /** Snapshot bytes through the provider before a host delivery channel opens them. */
  prepareFile?(path: string): Promise<{ path: string; size: number }>;
  /** Copy bytes from a host-authorized upload lease into private session storage. */
  importBytes?(name: string, bytes: Uint8Array): Promise<{ path: string; size: number }>;
};

/** Native Node provider for attachment delivery inspection. */
export function createNodeAttachmentDeliveryPort(): AttachmentDeliveryPort {
  return {
    realpath,
    async stat(path) {
      const value = await stat(path);
      return {
        size: value.size,
        kind: value.isFile() ? "file" : value.isDirectory() ? "directory" : "other",
      };
    },
  };
}
