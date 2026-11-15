import {
  FILE_HISTORY_PROJECTION_NAMES,
  FileHistoryStore,
  requireSessionProjectionValue,
  type AgentProjectSessionStorage,
  type FileHistorySnapshotProjectionResult,
} from "../session/index.js";
import type { FileHistoryFsPort } from "../session/filesystem/FileHistoryFsPort.js";

export type SessionFileHistoryBundleOptions = {
  fs?: FileHistoryFsPort;
  backupRoot?: string;
  sessionKey: string;
  storage: Pick<
    AgentProjectSessionStorage,
    "fileHistoryDir" | "fileHistoryBackupStorage" | "transcript" | "projections"
  >;
  now: () => Date;
};

/**
 * Composes the per-session file-history consumer over existing durable
 * storage. The storage runtime remains the sole event/projection owner.
 */
export class SessionFileHistoryBundle {
  constructor(private readonly options: SessionFileHistoryBundleOptions) {}

  compose(): FileHistoryStore {
    const fileHistory = new FileHistoryStore({
      backupDir: this.options.backupRoot ?? this.options.storage.fileHistoryDir,
      fs: this.options.fs,
      now: this.options.now,
      backupStorage: this.options.fs ? undefined : this.options.storage.fileHistoryBackupStorage,
      onSnapshotRecorded: (snapshot, snapshotKind) =>
        this.options.storage.transcript.recordFileHistorySnapshot(
          this.options.sessionKey,
          snapshot.messageId,
          snapshot,
          snapshotKind,
        ),
    });
    const projectionSnapshot = this.options.storage.projections.snapshot([
      FILE_HISTORY_PROJECTION_NAMES.snapshots,
    ]);
    fileHistory.replayFromTranscript(
      requireSessionProjectionValue<FileHistorySnapshotProjectionResult>(
        projectionSnapshot,
        FILE_HISTORY_PROJECTION_NAMES.snapshots,
      ).flatMap((entry) => {
        const timestamp = entry.timestamp ?? entry.snapshotTimestamp;
        return timestamp ? [{ ...entry, timestamp }] : [];
      }),
    );
    return fileHistory;
  }
}
