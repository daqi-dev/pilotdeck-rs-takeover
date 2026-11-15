/** File-history consumer operations, implemented within the session namespace. */
export type FileHistoryFsPort = {
  stat(path: string): Promise<{ size: number; mode: number; mtimeMs: number; isFile(): boolean }>;
  readFile(path: string): Promise<Buffer>;
  readFile(path: string, encoding: "utf8" | "utf-8"): Promise<string>;
  mkdir(path: string, options: { recursive: true }): Promise<unknown>;
  copyFile(source: string, destination: string): Promise<void>;
  chmod(path: string, mode: number): Promise<void>;
  unlink(path: string): Promise<void>;
  rename(source: string, destination: string): Promise<void>;
  access(path: string): Promise<void>;
  writeFile(path: string, bytes: Uint8Array): Promise<void>;
};
