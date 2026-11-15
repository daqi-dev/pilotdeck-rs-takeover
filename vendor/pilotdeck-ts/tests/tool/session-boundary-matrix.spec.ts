import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { NsjailSessionExecutionProvider } from "../../src/sandbox/nsjail/NsjailProvider.js";
import { nextSessionExecutionBinding } from "../../src/sandbox/SessionExecutionStorage.js";

test("bidirectional storage, mutation, search, export and symlink race boundaries", {
  skip: process.platform !== "linux" || !process.env.PILOTDECK_NSJAIL_ROOTFS, timeout: 180_000,
}, async () => {
  const root = await mkdtemp(join(tmpdir(), "pd-boundary-"));
  const provider = new NsjailSessionExecutionProvider({ sessionsRoot: root,
    rootfs: process.env.PILOTDECK_NSJAIL_ROOTFS!, executable: process.env.PILOTDECK_NSJAIL_BIN,
    cgroupV2Root: process.env.PILOTDECK_NSJAIL_CGROUP_ROOT,
  });
  const cases: Array<Record<string, unknown>> = [];
  let failure: unknown;
  try {
    const bindings = await Promise.all(["a", "b"].map((key) => nextSessionExecutionBinding(root, key, { network: "deny" })));
    const handles = await Promise.all(bindings.map((binding) => provider.createSession(binding)));
    const targets: string[][] = [];
    const originals = new Map<string, { bytes: string; entries: string[] }>();
    for (const binding of bindings) {
      const files = Object.values(binding.storage).map((path) => join(path, ".boundary", "victim"));
      targets.push(files);
      for (const file of files) {
        await mkdir(dirname(file), { recursive: true });
        await writeFile(file, `PRIVATE_${binding.sessionKey}_${file}`);
        originals.set(file, { bytes: await readFile(file, "utf8"), entries: await readdir(dirname(file)) });
      }
    }
    const observe = async () => {
      const observations = [];
      for (const [path, original] of originals) {
        const bytes = await readFile(path, "utf8");
        const entries = await readdir(dirname(path));
        assert.equal(bytes, original.bytes, path);
        assert.deepEqual(entries, original.entries, path);
        observations.push({ path, bytes, entries, unchanged: true });
      }
      return observations;
    };
    for (let index = 0; index < 2; index++) {
      const handle = handles[index]!;
      const binding = bindings[index]!;
      const other = targets[1 - index]!;
      const startedAt = new Date().toISOString();
      const variants = other.flatMap((path) => [path, path.replaceAll("/", "//"), `${dirname(path)}/../.boundary/victim`]);
      const script = `import os, errno, pathlib, subprocess
targets=${JSON.stringify(variants)}
results=[]
for p in targets:
 operations={
  'read':lambda: open(p).read(), 'list':lambda:os.listdir(os.path.dirname(p)),
  'overwrite':lambda:open(p,'w').write('BAD'), 'append':lambda:open(p,'a').write('BAD'),
  'delete':lambda:os.unlink(p), 'rename':lambda:os.rename(p,p+'.renamed'),
  'create':lambda:open(p+'.new','w').write('BAD'), 'hardlink':lambda:os.link(p,'/workspace/hardlink'),
 }
 for name, operation in operations.items():
  try: operation()
  except OSError as e:
   assert e.errno in (errno.ENOENT,errno.EACCES,errno.EPERM,errno.EXDEV), (name,p,e)
   results.append([name,p,e.errno])
  else: raise AssertionError((name,p,'escaped'))
for p in ${JSON.stringify(other)}:
 for command in [['cat',p],['rg','PRIVATE',os.path.dirname(p)]]:
  r=subprocess.run(command,capture_output=True,text=True)
  assert r.returncode != 0 and 'PRIVATE_' not in r.stdout, r
print('MATRIX_OK',len(results))
`;
      const result = await handle.world.subprocess.executeFile!({ executable: "python3", args: ["-c", script], cwd: binding.storage.workspace, env: {}, timeoutMs: 30_000 });
      assert.equal(result.exitCode, 0, result.stderr);
      assert.match(result.stdout, /MATRIX_OK/);
      for (const path of variants) {
        await assert.rejects(async () => handle.world.fs.readFile(path));
        await assert.rejects(async () => handle.world.fs.writeText(path, "BAD", { allowOverwrite: true }));
        await assert.rejects(async () => handle.world.attachmentDelivery.prepareFile!(path));
      }
      cases.push({ caseId: `ISO-MATRIX-${binding.sessionKey}`, status: "PASS", ...binding,
        request: { executable: "python3", script, variants, channels: ["shell", "python", "fs", "rg", "attachment"] }, response: result,
        exitCode: result.exitCode, startedAt, finishedAt: new Date().toISOString(), hostObservation: await observe(), failureReason: null });
      const race = await handle.world.detachedShell.start({
        command: `python3 -c ${quote(`import os,time
open('/workspace/race-started','w').write('READY')
end=time.monotonic()+120
while time.monotonic()<end:
 for target in ['/workspace/own',${JSON.stringify(other[0])}]:
  try: os.unlink('/workspace/racing')
  except FileNotFoundError: pass
  os.symlink(target,'/workspace/racing')
`)}`, cwd: binding.storage.workspace, env: {},
      });
      const raceStarted = new Date().toISOString();
      for (let attempt = 0; ; attempt++) {
        if (await readFile(join(binding.storage.workspace, "race-started"), "utf8").catch(() => "") === "READY") break;
        assert.ok(attempt < 100, "race worker did not start");
        await new Promise((resolve) => setTimeout(resolve, 20));
      }
      await handle.world.fs.writeText(join(binding.storage.workspace, "own"), "OWN");
      const outcomes: unknown[] = [];
      try {
        for (let i = 0; i < 20; i++) {
          for (const operation of [
            () => handle.world.fs.readFile(join(binding.storage.workspace, "racing"), { encoding: "utf8" }),
            () => handle.world.fs.writeText(join(binding.storage.workspace, "racing"), "OWN", { allowOverwrite: true }),
            async () => { const artifact = await handle.world.attachmentDelivery.prepareFile!(join(binding.storage.workspace, "racing")); return readFile(artifact.path, "utf8"); },
          ]) {
            try { const value = await operation(); assert.doesNotMatch(JSON.stringify(value), /PRIVATE_/); outcomes.push({ allowedOwn: value }); }
            catch (error) { if (error instanceof assert.AssertionError) throw error; outcomes.push({ denied: String(error) }); }
          }
        }
      } finally { race.terminate("SIGKILL"); await race.exit; }
      cases.push({ caseId: `ISO-RACE-${binding.sessionKey}`, status: "PASS", sessionKey: binding.sessionKey, sandboxKey: binding.sandboxKey, generation: 1,
        request: { iterations: 20, operations: ["read", "write", "export"], target: other[0] }, response: outcomes, exitCode: 0,
        startedAt: raceStarted, finishedAt: new Date().toISOString(), hostObservation: await observe(), failureReason: null });
      const osProbe = await handle.world.shell.execute({
        command: `test ! -e /proc/${process.pid}/root && test ! -e /proc/${process.pid}/fd && test ! -e /dev/mem && ! mount -t tmpfs none /mnt && ! unshare -m true && ! touch /proc/pilotdeck-probe && printf OS_BOUNDARY_OK`,
        cwd: binding.storage.workspace, env: {}, timeoutMs: 10_000,
      });
      assert.equal(osProbe.exitCode, 0, osProbe.stderr);
      const network = await handle.world.subprocess.executeFile!({ executable: "python3", args: ["-c", `import socket
for host,port in [('1.1.1.1',443),('127.0.0.1',60008),('127.0.0.1',29876)]:
 try: socket.create_connection((host,port),timeout=.3)
 except OSError: pass
 else: raise AssertionError('network escaped')
print('NETWORK_DENIED')`], cwd: binding.storage.workspace, env: {}, timeoutMs: 5_000 });
      assert.equal(network.exitCode, 0, network.stderr);
      cases.push({ caseId: `ISO-OS-NET-${binding.sessionKey}`, status: "PASS", sessionKey: binding.sessionKey, sandboxKey: binding.sandboxKey, generation: 1,
        request: { command: "OS capability and raw socket probes" }, response: { osProbe, network }, exitCode: 0,
        startedAt, finishedAt: new Date().toISOString(), hostObservation: await observe(), failureReason: null });
    }
  } catch (error) { failure = error; throw error; }
  finally {
    await provider.dispose();
    const artifacts = process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR;
    if (artifacts) { await mkdir(artifacts, { recursive: true }); await writeFile(join(artifacts, "boundary-matrix.json"), JSON.stringify({ status: failure ? "FAIL" : "PASS", cases, failureReason: failure ? String(failure) : null }, null, 2)); }
    await rm(root, { recursive: true, force: true });
  }
});

function quote(value: string) { return `'${value.replaceAll("'", "'\\''")}'`; }
