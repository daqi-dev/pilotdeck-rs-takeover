import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import assert from 'node:assert/strict';

const digest = value => createHash('sha256').update(value).digest('hex');
const git = (cwd, args, encoding = 'utf8') => {
  const result = spawnSync('git', args, { cwd, encoding, maxBuffer: 64 * 1024 * 1024 });
  if (result.error || result.status !== 0) throw result.error ?? new Error(`git ${args[0]} failed: ${result.stderr}`);
  return result.stdout;
};

export function artifact(file) {
  const absolute = file instanceof URL ? fileURLToPath(file) : path.resolve(file);
  const bytes = fs.readFileSync(absolute);
  return { path: absolute, sha256: digest(bytes), size: bytes.length };
}

export function assertArtifactsUnchanged(artifacts) {
  for (const expected of artifacts) {
    assert.deepEqual(artifact(expected.path), expected, `artifact changed during validation: ${expected.path}`);
  }
}

export function sourceSnapshot(repo) {
  const status = git(repo, ['status', '--porcelain=v1', '--untracked-files=all', '--', 'src/']).trimEnd();
  const untracked = git(repo, ['ls-files', '--others', '--exclude-standard', '-z', '--', 'src/'])
    .split('\0').filter(file => file.endsWith('.rs')).sort();
  return {
    head: git(repo, ['rev-parse', 'HEAD']).trim(),
    kind: status ? 'development source snapshot' : 'clean commit',
    dirty: Boolean(status),
    trackedSrcDiffSha256: digest(git(repo, ['diff', '--binary', 'HEAD', '--', 'src/'], null)),
    untrackedRust: untracked.map(file => ({ path: file, sha256: artifact(path.join(repo, file)).sha256 })),
    status: status ? status.split('\n') : [],
  };
}

export function validationIdentity({ baseline, rust, scenario }) {
  const harnessRepo = fileURLToPath(new URL('../..', import.meta.url));
  const repo = path.resolve(process.env.PDRS_VALIDATION_SOURCE || harnessRepo);
  const archivedCommit = path.join(harnessRepo, 'source-commit.txt');
  const harnessCommit = fs.existsSync(path.join(harnessRepo, '.git'))
    ? git(harnessRepo, ['rev-parse', 'HEAD']).trim()
    : fs.existsSync(archivedCommit) ? fs.readFileSync(archivedCommit, 'utf8').trim() : null;
  const scenarioArtifact = artifact(scenario);
  const helper = artifact(new URL(import.meta.url));
  const tsEntry = artifact(path.join(baseline, 'dist/src/cli/pilotdeck.js'));
  const tsLock = artifact(path.join(baseline, 'pnpm-lock.yaml'));
  const rustArtifact = rust ? artifact(rust) : null;
  const rustSource = sourceSnapshot(repo);
  return {
    fields: {
      scenarioSha256: scenarioArtifact.sha256,
      identityHelperSha256: helper.sha256,
      harnessCommit,
      rustSourceRoot: repo,
      tsCommit: git(baseline, ['rev-parse', 'HEAD']).trim(),
      tsArtifacts: { entry: tsEntry, lockfile: tsLock },
      rustCommit: rustSource.head,
      rustSource,
      rustBinary: rust,
      rustSha256: rustArtifact?.sha256 ?? null,
    },
    finish() {
      assertArtifactsUnchanged([scenarioArtifact, helper, tsEntry, tsLock, ...(rustArtifact ? [rustArtifact] : [])]);
      return { artifactsUnchanged: true, rustSourceAtEnd: sourceSnapshot(repo) };
    },
  };
}
