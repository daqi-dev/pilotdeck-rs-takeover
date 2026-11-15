import { randomUUID } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import { basename, join } from "node:path";
import type { FsPort } from "../../tool/execution-world/FsPort.js";
import type { AttachmentDeliveryPort } from "../../tool/execution-world/AttachmentDeliveryPort.js";
import type { FileHistoryFsPort } from "../../session/filesystem/FileHistoryFsPort.js";

/** Guest resolution stays in nsjail; the host channel receives a private snapshot. */
export function createNsjailAttachmentDeliveryPort(fs: FsPort & { fileHistoryFs: FileHistoryFsPort }, exportRoot: string, importRoot: string): AttachmentDeliveryPort {
  return {
    realpath: (path) => fs.realpath!(path),
    stat: (path) => fs.stat(path),
    async importBytes(name, bytes) {
      if (bytes.byteLength > 64 * 1024 * 1024) throw new Error("Session upload exceeds 64 MiB");
      const root = join(importRoot, randomUUID());
      const leaf = basename(name);
      const path = join(root, !leaf || leaf === "." || leaf === ".." ? "attachment" : leaf);
      await fs.fileHistoryFs.mkdir(root, { recursive: true });
      await fs.fileHistoryFs.writeFile(path, bytes);
      return { path, size: bytes.byteLength };
    },
    async prepareFile(path) {
      const metadata = await fs.stat(path);
      if (metadata.kind !== "file" || metadata.size > 64 * 1024 * 1024) {
        throw new Error("Attachment must be a regular file of at most 64 MiB");
      }
      const bytes = await fs.readFile(path);
      const content = typeof bytes === "string" ? Buffer.from(bytes) : bytes;
      if (content.byteLength > 64 * 1024 * 1024) throw new Error("Attachment exceeds 64 MiB");
      const root = join(exportRoot, randomUUID());
      await mkdir(root, { recursive: true, mode: 0o700 });
      const snapshot = join(root, basename(path));
      await writeFile(snapshot, content, { flag: "wx", mode: 0o400 });
      return { path: snapshot, size: content.byteLength };
    },
  };
}
