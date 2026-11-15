import assert from "node:assert/strict";
import { mkdir, readFile, readdir, stat, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

import { NsjailSessionExecutionProvider } from "../../src/sandbox/nsjail/NsjailProvider.js";
import type { TrustedSessionBinding } from "../../src/sandbox/SessionExecutionProvider.js";
import { createExecuteCodeTool } from "../../src/tool/builtin/executeCode.js";
import { attachCaseEvidence, observeExecution, type ExecutionObservation } from "../fixtures/session-execution-evidence.js";
import { nsjailCgroupSessionKey } from "../../src/sandbox/nsjail/NsjailCgroupLease.js";

const rootfs = process.env.PILOTDECK_NSJAIL_ROOTFS;
const executable = process.env.PILOTDECK_NSJAIL_BIN ?? "nsjail";
const skipReason = process.platform !== "linux"
  ? "real nsjail integration requires Linux"
  : !rootfs
    ? "set PILOTDECK_NSJAIL_ROOTFS to run the real nsjail integration"
    : !isExecutableAvailable(executable)
      ? `nsjail executable is unavailable: ${executable}`
      : undefined;

test("real nsjail isolates concurrent sessions and resumes persistent state", { skip: skipReason ?? false }, async () => {
  const runRoot = await mkdirTemp("pilotdeck-nsjail-acceptance-very-long-session-storage-path-for-private-uds-");
  const sessionsRoot = join(runRoot, "sessions");
  await mkdir(sessionsRoot, { recursive: true });
  const artifactDir = process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR;
  const cases: Array<Record<string, unknown>> = [];
  const executionObservations: ExecutionObservation[] = [];
  attachCaseEvidence(cases, executionObservations);
  const provider = new NsjailSessionExecutionProvider({
    executable,
    rootfs: rootfs!,
    sessionsRoot,
    sandboxMode: "danger-full-access",
  });
  const create = provider.createSession.bind(provider);
  provider.createSession = async (binding) => observeExecution(await create(binding), binding, executionObservations);
  let firstA: Awaited<ReturnType<typeof provider.createSession>> | undefined;
  let sessionB: Awaited<ReturnType<typeof provider.createSession>> | undefined;
  let resumedA: Awaited<ReturnType<typeof provider.createSession>> | undefined;
  let capacityProvider: NsjailSessionExecutionProvider | undefined;
  const capacityHandles: Array<Awaited<ReturnType<typeof provider.createSession>>> = [];
  let pythonCommand: string | undefined;
  let failure: unknown;

  try {
    const readiness = await provider.probe();
    assert.equal(readiness.ready, true, readiness.reason ?? "nsjail probe failed");
    cases.push({ id: "ENV-READY", status: "PASS", readiness, request: { operation: "provider.probe" }, response: readiness });

    const bindingA = makeBinding(sessionsRoot, "session-a", 1);
    const bindingB = makeBinding(sessionsRoot, "session-b", 1);
    [firstA, sessionB] = await Promise.all([
      provider.createSession(bindingA),
      provider.createSession(bindingB),
    ]);
    const activeA = firstA;
    assert.ok(activeA);
    const environment = await runShell(activeA, bindingA.storage.workspace,
      "id; node --version; python3 --version; python3 -m pip --version; npm --version; rg --version; cat /proc/self/status; cat /proc/self/cgroup; cat /proc/self/mountinfo; printf '\\nHOME=%s\\nTMPDIR=%s\\nPYTHONUSERBASE=%s\\nPIP_CACHE_DIR=%s\\nNPM_CONFIG_PREFIX=%s\\nNPM_CONFIG_CACHE=%s\\nNPM_CONFIG_USERCONFIG=%s\\n' \"$HOME\" \"$TMPDIR\" \"$PYTHONUSERBASE\" \"$PIP_CACHE_DIR\" \"$NPM_CONFIG_PREFIX\" \"$NPM_CONFIG_CACHE\" \"$NPM_CONFIG_USERCONFIG\"");
    assert.match(environment, /CapEff:\s+0+\b/);
    cases.push({ id: "ENV-CONFIG", status: "PASS", sessionKey: bindingA.sessionKey, sandboxKey: bindingA.sandboxKey, generation: 1,
      response: environment, detail: "guest versions, uid/gid, capabilities, seccomp, namespace mounts and package paths" });
    await assert.rejects(provider.deleteSessionStorage(bindingA.sessionKey), /active/);
    process.env.PILOTDECK_ACCEPTANCE_HOST_SECRET = "HOST_CREDENTIAL_SENTINEL";
    try {
      const envProbe = await activeA.world.shell.execute({
        command: "node -e \"const fs=require('fs'); if(fs.readFileSync('/proc/self/environ').includes('HOST_CREDENTIAL_SENTINEL'))process.exit(2); process.stdout.write(process.env.SESSION_ONLY_VAR)\"",
        cwd: bindingA.storage.workspace, env: { ...process.env, SESSION_ONLY_VAR: "SESSION_ALLOWED" }, timeoutMs: 10_000,
      });
      assert.equal(envProbe.exitCode, 0, envProbe.stderr);
      assert.equal(envProbe.stdout, "SESSION_ALLOWED");
    } finally { delete process.env.PILOTDECK_ACCEPTANCE_HOST_SECRET; }
    cases.push({ id: "ISO-HOST-ENV", status: "PASS", detail: "host environment credentials are absent from guest environ; session-specific variables remain available" });

    await runShell(firstA, bindingA.storage.workspace, "printf 'A_MARKER' > same.txt; export SESSION_ENV=OK");
    await runShell(sessionB, bindingB.storage.workspace, "printf 'B_MARKER' > same.txt; export SESSION_ENV=B_ONLY");
    assert.equal(await runShell(firstA, bindingA.storage.workspace, "cat same.txt"), "A_MARKER");
    assert.equal(await runShell(sessionB, bindingB.storage.workspace, "cat same.txt"), "B_MARKER");
    cases.push({ id: "CORE-02", status: "PASS", detail: "same path is private per session" });

    const aPath = join(bindingA.storage.workspace, "same.txt");
    const bPath = join(bindingB.storage.workspace, "same.txt");
    await expectDenied(sessionB, bindingB.storage.workspace, `cat ${shellQuote(aPath)}`, "A workspace read from B");
    await expectDenied(firstA, bindingA.storage.workspace, `cat ${shellQuote(bPath)}`, "B workspace read from A");
    await expectDenied(firstA, bindingA.storage.workspace, `printf BAD >> ${shellQuote(bPath)}`, "B workspace write from A");
    await expectDenied(firstA, bindingA.storage.workspace, `cat ${shellQuote(join(bindingB.storage.workspace, "..", "session-b", "workspace", "same.txt"))}`, "B normalized workspace read from A");
    assert.equal(await readFile(bPath, "utf8"), "B_MARKER");
    const aLinkResult = await firstA.world.shell.execute({
      command: `ln -s ${shellQuote(bPath)} escape-link 2>/dev/null || true`,
      cwd: bindingA.storage.workspace,
      env: baseEnv(),
      timeoutMs: 20_000,
    });
    assert.equal(aLinkResult.exitCode, 0);
    await expectDenied(firstA, bindingA.storage.workspace, "cat escape-link", "symlink read from A to B");
    cases.push({ id: "ISO-01/02/03/04", status: "PASS", detail: "absolute, normalized and symlink cross-session access denied" });

    await runShell(firstA, bindingA.storage.workspace, "mkdir -p \"$HOME/.pilotdeck/control\" \"$HOME/.pilotdeck/spill\" \"$HOME/.pilotdeck/artifact\"; printf A_CONTROL > \"$HOME/.pilotdeck/control/marker\"; printf A_SPILL > \"$HOME/.pilotdeck/spill/marker\"; printf A_ARTIFACT > \"$HOME/.pilotdeck/artifact/marker\"");
    await runShell(sessionB, bindingB.storage.workspace, "mkdir -p \"$HOME/.pilotdeck/control\" \"$HOME/.pilotdeck/spill\" \"$HOME/.pilotdeck/artifact\"; printf B_CONTROL > \"$HOME/.pilotdeck/control/marker\"; printf B_SPILL > \"$HOME/.pilotdeck/spill/marker\"; printf B_ARTIFACT > \"$HOME/.pilotdeck/artifact/marker\"");
    for (const relativePath of [".pilotdeck/control/marker", ".pilotdeck/spill/marker", ".pilotdeck/artifact/marker"]) {
      await expectDenied(firstA, bindingA.storage.workspace, `cat ${shellQuote(join(bindingB.storage.home, relativePath))}`, `B ${relativePath} read from A`);
      await expectDenied(firstA, bindingA.storage.workspace, `printf BAD >> ${shellQuote(join(bindingB.storage.home, relativePath))}`, `B ${relativePath} write from A`);
    }
    await expectDenied(firstA, bindingA.storage.workspace, `ln ${shellQuote(bPath)} hard-link 2>/dev/null`, "hard link into B workspace");
    await expectDenied(firstA, bindingA.storage.workspace, `cat /proc/${process.pid}/environ`, "host process environment disclosure");
    await expectDenied(firstA, bindingA.storage.workspace, `ls /proc/${process.pid}/fd`, "host management descriptors");
    await expectDenied(firstA, bindingA.storage.workspace, `cat /proc/1/root${shellQuote(bPath)}`, "proc root escape to another session");
    const mounts = await runShell(firstA, bindingA.storage.workspace, "cat /proc/self/mountinfo");
    assert.doesNotMatch(mounts, new RegExp(bindingB.sandboxKey));
    const hostPidNamespace = (await stat("/proc/self/ns/pid")).ino;
    const guestPidNamespace = Number(await runShell(firstA, bindingA.storage.workspace, "node -e \"process.stdout.write(String(require('fs').statSync('/proc/self/ns/pid').ino))\""));
    assert.notEqual(guestPidNamespace, hostPidNamespace);
    await expectDenied(firstA, bindingA.storage.workspace, "ls /var/lib/pilotdeck/sessions", "session parent listing");
    cases.push({ id: "ISO-05/06/07/IO-01/STATE-01", status: "PASS", detail: "control, spill, artifact, hard-link, proc and session-parent probes are denied" });

    pythonCommand = await findCommand(firstA, bindingA.storage.workspace, "python3")
      ? "python3"
      : await findCommand(firstA, bindingA.storage.workspace, "python3.12")
        ? "python3.12"
        : undefined;
    const nodeAvailable = await findCommand(firstA, bindingA.storage.workspace, "node");
    const rgAvailable = await findCommand(firstA, bindingA.storage.workspace, "rg");
    const npmAvailable = await findCommand(firstA, bindingA.storage.workspace, "npm");
    const missingTools = [
      ...(pythonCommand ? [] : ["python"]),
      ...(nodeAvailable ? [] : ["node"]),
      ...(rgAvailable ? [] : ["rg"]),
      ...(npmAvailable ? [] : ["npm"]),
    ];
    cases.push({
      id: "ENV-TOOLS",
      status: missingTools.length === 0 ? "PASS" : "BLOCKED",
      detail: missingTools.length === 0 ? "shell, Python, Node, npm and rg are available" : `missing: ${missingTools.join(", ")}`,
    });
    if (nodeAvailable) {
      assert.equal(await runShell(firstA, bindingA.storage.workspace, "node -e \"process.stdout.write('NODE_OK')\""), "NODE_OK");
    }
    if (rgAvailable) {
      assert.equal(await runShell(firstA, bindingA.storage.workspace, "printf 'RG_OK\\n' > rg-marker.txt; rg -n RG_OK rg-marker.txt | cut -d: -f2"), "RG_OK");
    }
    if (npmAvailable && nodeAvailable) {
      for (const [handle, binding, marker, version] of [
        [firstA, bindingA, "A", "1.0.0"], [sessionB, bindingB, "B", "2.0.0"],
      ] as const) {
        const packageDir = join(binding.storage.workspace, "npm-marker-package");
        await mkdir(packageDir, { recursive: true });
        await writeFile(join(packageDir, "package.json"), JSON.stringify({
          name: "pilotdeck-session-npm-marker", version, main: "index.js", scripts: { install: "node install.js" },
        }));
        await writeFile(join(packageDir, "index.js"), `module.exports = { value: '${marker}_ONLY', native: require('./marker.node') };\n`);
        await writeFile(join(packageDir, "marker.c"), `typedef void *napi_env; typedef void *napi_value;\nextern int napi_create_string_utf8(napi_env, const char *, unsigned long, napi_value *);\nnapi_value napi_register_module_v1(napi_env env, napi_value exports) { napi_value result; napi_create_string_utf8(env, "${marker}_NATIVE", 8, &result); return result; }\n`);
        await writeFile(join(packageDir, "install.js"), `const fs = require('node:fs'); const cp = require('node:child_process');\nif (process.env.HOME !== '/home/agent' || fs.existsSync(${JSON.stringify(sessionsRoot)})) throw new Error('lifecycle escaped session');\ncp.execFileSync('gcc', ['-shared', '-fPIC', '-nostdlib', '-o', 'marker.node', 'marker.c']);\nfs.writeFileSync(process.env.HOME + '/npm-lifecycle.txt', '${marker}_LIFECYCLE');\n`);
        await runShell(handle, binding.storage.workspace, `printf '${marker}_CONFIG' > "$NPM_CONFIG_USERCONFIG"; npm install --prefix "$NPM_CONFIG_PREFIX" --no-audit --no-fund --offline ./npm-marker-package`);
        assert.equal(await runShell(handle, binding.storage.workspace, "node -e \"const p=require('/home/agent/.local/npm/node_modules/pilotdeck-session-npm-marker'); process.stdout.write(p.value+':'+p.native+':'+require('/home/agent/.local/npm/node_modules/pilotdeck-session-npm-marker/package.json').version)\""), `${marker}_ONLY:${marker}_NATIVE:${version}`);
        assert.equal(await runShell(handle, binding.storage.workspace, 'cat "$HOME/npm-lifecycle.txt"; cat "$NPM_CONFIG_USERCONFIG"'), `${marker}_LIFECYCLE${marker}_CONFIG`);
        await expectDenied(handle, binding.storage.workspace, "npm install --global --prefix /usr/local --offline ./npm-marker-package", "global npm cannot modify shared rootfs");
      }
      cases.push({ id: "ENV-NPM-01", status: "PASS", detail: "same-name different-version npm packages, config, lifecycle scripts and native addon compilation are session-private" });
    } else {
      cases.push({ id: "ENV-NPM-01", status: "BLOCKED", detail: "npm is missing from the configured rootfs" });
    }

    for (const command of ["apt", "apt-get", "apt-cache", "dpkg", "dpkg-deb", "dpkg-query"]) {
      await expectDenied(firstA, bindingA.storage.workspace, `${command} --version`, `${command} is not executable`);
    }
    await expectDenied(firstA, bindingA.storage.workspace, "apt-get update", "apt metadata update");
    await expectDenied(firstA, bindingA.storage.workspace, "apt-get install -y pilotdeck-nonexistent-package", "apt package install");
    for (const path of ["/usr/pilotdeck-probe", "/lib/pilotdeck-probe", "/var/lib/dpkg/pilotdeck-probe", "/var/lib/dpkg/lock"]) {
      await expectDenied(firstA, bindingA.storage.workspace, `printf BAD > ${path}`, `system write ${path}`);
    }
    cases.push({ id: "APT-N/A", status: "N/A", detail: "all six apt/dpkg commands and system/package-database writes were attempted and denied" });

    assert.equal(await runShell(firstA, bindingA.storage.workspace, `test ! -e /proc/${process.pid}`), "");
    cases.push({ id: "ISO-05/06", status: "PASS", detail: "read-only proc uses an independent PID namespace; Gateway PID, FD, environment and foreign mount paths are absent", hostPidNamespace, guestPidNamespace });

    assert.equal(await runShell(firstA, bindingA.storage.workspace, "printf '%s' \"$SESSION_ENV\""), "OK");
    cases.push({ id: "STATE-01", status: "PASS", detail: "environment file is restored between shell calls" });

    await runShell(firstA, bindingA.storage.workspace, "mkdir -p \"$HOME/.local\" \"$PIP_CACHE_DIR\"; printf 'A_HOME' > \"$HOME/home-marker.txt\"; printf 'A_TMP' > \"$TMPDIR/tmp-marker.txt\"; printf 'A_CACHE' > \"$PIP_CACHE_DIR/cache-marker.txt\"");
    await runShell(sessionB, bindingB.storage.workspace, "mkdir -p \"$HOME/.local\" \"$PIP_CACHE_DIR\"; printf 'B_HOME' > \"$HOME/home-marker.txt\"; printf 'B_TMP' > \"$TMPDIR/tmp-marker.txt\"; printf 'B_CACHE' > \"$PIP_CACHE_DIR/cache-marker.txt\"");
    assert.equal(await runShell(firstA, bindingA.storage.workspace, "cat \"$HOME/home-marker.txt\"; cat \"$TMPDIR/tmp-marker.txt\"; cat \"$PIP_CACHE_DIR/cache-marker.txt\""), "A_HOMEA_TMPA_CACHE");
    assert.equal(await runShell(sessionB, bindingB.storage.workspace, "cat \"$HOME/home-marker.txt\"; cat \"$TMPDIR/tmp-marker.txt\"; cat \"$PIP_CACHE_DIR/cache-marker.txt\""), "B_HOMEB_TMPB_CACHE");
    cases.push({ id: "ISO-07", status: "PASS", detail: "home, tmp and pip cache are private" });

    await activeA.world.fs.writeText(join(bindingA.storage.workspace, "fs-marker.txt"), "FS_A");
    assert.equal(await activeA.world.fs.readFile(join(bindingA.storage.workspace, "fs-marker.txt"), { encoding: "utf8" }), "FS_A");
    assert.throws(() => activeA.world.fs.readFile(bPath), /outside the session workspace/);
    cases.push({ id: "CORE-01", status: "PASS", detail: "session filesystem read/write is owner-bound" });

    const planDir = join(bindingA.storage.workspace, ".pilotdeck", "plans");
    activeA.world.planStorage.ensureDirectory(planDir);
    await activeA.world.fs.writeText(join(planDir, "plan.md"), "A_PLAN");
    assert.equal(activeA.world.planStorage.readText(join(planDir, "plan.md")), "A_PLAN");
    assert.throws(() => activeA.world.planStorage.readText(bPath), /outside the session workspace/);
    const delivery = activeA.world.attachmentDelivery;
    assert.equal(await delivery.realpath(aPath), aPath);
    await assert.rejects(delivery.realpath(join(bindingA.storage.workspace, "escape-link")));
    const snapshot = await delivery.prepareFile!(aPath);
    await activeA.world.fs.writeText(aPath, "A_CHANGED", { allowOverwrite: true });
    assert.equal(await readFile(snapshot.path, "utf8"), "A_MARKER");
    await activeA.world.fs.writeText(aPath, "A_MARKER", { allowOverwrite: true });
    await assert.rejects(delivery.stat(bPath), /outside the session workspace/);
    cases.push({ id: "EXT-06-PLAN-ARTIFACT", status: "PASS", sessionKey: bindingA.sessionKey, sandboxKey: bindingA.sandboxKey, generation: bindingA.generation, detail: "plan operations run in nsjail and attachment delivery uses private immutable bytes" });

    if (pythonCommand) {
      const tool = createExecuteCodeTool({
        executionWorkspace: activeA.world.executionWorkspace,
        executionTransport: activeA.world.executionTransport,
        codeRuntime: activeA.world.codeRuntime,
        sandbox: activeA.world.executeCodeSandbox,
      });
      const helperCalls: string[] = [];
      const codeToolResult = await tool.execute({ code: "from pilotdeck_tools import read_file\nprint(read_file(file_path='same.txt'))" }, {
        sessionId: bindingA.sessionKey, turnId: "helper-rpc", cwd: bindingA.storage.workspace, env: baseEnv(),
        permissionMode: "bypassPermissions",
        permissionContext: { mode: "bypassPermissions", cwd: bindingA.storage.workspace, additionalWorkingDirectories: [], canPrompt: false, bypassAvailable: false, rules: { allow: [], deny: [], ask: [] } },
        executeTool: async (call) => {
          helperCalls.push(call.name);
          return { type: "success", toolCallId: call.id, toolName: call.name, startedAt: new Date().toISOString(), completedAt: new Date().toISOString(), content: [{ type: "text", text: String(await activeA.world.fs.readFile(aPath, { encoding: "utf8" })) }] };
        },
      });
      assert.equal(codeToolResult.data?.status, "success", JSON.stringify(codeToolResult));
      assert.match(String(codeToolResult.data?.output), /A_MARKER/);
      assert.deepEqual(helperCalls, ["read_file"]);
      assert.ok(Buffer.byteLength(join(bindingA.storage.temp, "r-123456789012")) > 108);
      cases.push({ id: "EXT-01-HELPER", status: "PASS", sessionKey: bindingA.sessionKey, sandboxKey: bindingA.sandboxKey, generation: bindingA.generation, detail: "execute_code and its private UDS helper RPC use the session world" });

      const codeRun = await runCode(firstA, bindingA.storage.workspace, pythonCommand, ["-c", "print('CODE_OK')"]);
      assert.equal(codeRun.stdout.trim(), "CODE_OK", JSON.stringify(codeRun));
      const codeCrossRead = await runCode(firstA, bindingA.storage.workspace, pythonCommand, [
        "-c",
        `p=${JSON.stringify(bPath)}; import pathlib; print('LEAK' if pathlib.Path(p).read_text() else 'LEAK')`,
      ]);
      assert.notEqual(codeCrossRead.exitCode, 0);
      assert.doesNotMatch(`${codeCrossRead.stdout}\n${codeCrossRead.stderr}`, /B_MARKER/);
      cases.push({ id: "EXT-01", status: "PASS", detail: "code runtime uses the session sandbox" });

      const abortController = new AbortController();
      const cancelledRun = runCode(firstA, bindingA.storage.workspace, pythonCommand, ["-c", "import time; time.sleep(30)"], abortController.signal);
      const siblingRun = runCode(firstA, bindingA.storage.workspace, pythonCommand, ["-c", "import time; time.sleep(.3); print('A_SURVIVES')"]);
      setTimeout(() => abortController.abort(), 150).unref();
      const cancelledResult = await cancelledRun;
      assert.equal(cancelledResult.cancelled, true);
      assert.equal((await siblingRun).stdout.trim(), "A_SURVIVES");
      assert.equal(await runShell(sessionB, bindingB.storage.workspace, "printf '%s' \"$SESSION_ENV\""), "B_ONLY");
      cases.push({ id: "LIFE-02", status: "PASS", detail: "cancelling one A request leaves another A request and B alive" });
      const noisy = await runCode(firstA, bindingA.storage.workspace, pythonCommand, ["-c", "import sys; sys.stdout.write('A'*4000000); sys.stderr.write('ERR_A'*800000)"]);
      assert.equal(noisy.exitCode, 0);
      assert.ok(Buffer.byteLength(noisy.stdout) < 70 * 1024);
      assert.ok(Buffer.byteLength(noisy.stderr) <= 64 * 1024);
      assert.doesNotMatch(noisy.stdout + noisy.stderr, /B_MARKER/);
      const shellNoise = await firstA.world.shell.execute({ command: `${pythonCommand} -c "import sys;sys.stdout.write('A'*4000000);sys.stderr.write('ERR_A'*800000)"`, cwd: bindingA.storage.workspace, env: baseEnv(), timeoutMs: 10_000 });
      assert.equal(shellNoise.exitCode, 0);
      assert.ok(Buffer.byteLength(shellNoise.stdout) <= 1024 * 1024);
      assert.ok(Buffer.byteLength(shellNoise.stderr) <= 1024 * 1024);
      assert.equal(await runShell(sessionB, bindingB.storage.workspace, "printf B_OUTPUT"), "B_OUTPUT");
      cases.push({ id: "LIMIT-OUTPUT", status: "PASS", detail: "code and shell/subprocess capture and progress remain bounded; session output does not mix" });

      const packageRoot = join(bindingA.storage.workspace, "marker-package");
      await mkdir(packageRoot, { recursive: true });
      await writeFile(join(packageRoot, "setup.py"), "from setuptools import setup\nsetup(name='pilotdeck-session-marker', version='1.0.0', py_modules=['pilotdeck_session_marker'])\n");
      await writeFile(join(packageRoot, "pilotdeck_session_marker.py"), "VALUE = 'A_ONLY'\n");
      await runShell(firstA, bindingA.storage.workspace, `mkdir -p \"$HOME/.local\" \"$HOME/.cache/pip\"; ${pythonCommand} -m pip install --user --no-deps --no-build-isolation ./marker-package`);
      assert.equal(await runShell(firstA, bindingA.storage.workspace, `${pythonCommand} -c 'import pilotdeck_session_marker as m; print(m.VALUE)'`), "A_ONLY");
      assert.equal(
        await runShell(sessionB, bindingB.storage.workspace, `${pythonCommand} -c 'import importlib.util; print(\"LEAK\" if importlib.util.find_spec(\"pilotdeck_session_marker\") else \"PRIVATE\")'`),
        "PRIVATE",
      );
      const bPackageRoot = join(bindingB.storage.workspace, "marker-package");
      await mkdir(bPackageRoot, { recursive: true });
      await writeFile(join(bPackageRoot, "setup.py"), "from setuptools import setup\nsetup(name='pilotdeck-session-marker', version='2.0.0', py_modules=['pilotdeck_session_marker'])\n");
      await writeFile(join(bPackageRoot, "pilotdeck_session_marker.py"), "VALUE = 'B_ONLY'\n");
      await runShell(sessionB, bindingB.storage.workspace, `${pythonCommand} -m pip install --user --no-index --no-deps --no-build-isolation ./marker-package`);
      assert.equal(await runShell(sessionB, bindingB.storage.workspace, `${pythonCommand} -c 'import pilotdeck_session_marker as m; from importlib.metadata import version; print(m.VALUE + ":" + version("pilotdeck-session-marker"))'`), "B_ONLY:2.0.0");
      assert.equal(await runShell(firstA, bindingA.storage.workspace, `${pythonCommand} -c 'import pilotdeck_session_marker as m; from importlib.metadata import version; print(m.VALUE + ":" + version("pilotdeck-session-marker"))'`), "A_ONLY:1.0.0");
      cases.push({ id: "ENV-02", status: "PASS", detail: "same-name Python packages at versions 1.0.0 and 2.0.0 remain private" });
      const networkProbe = await runShell(firstA, bindingA.storage.workspace, `${pythonCommand} -c 'import socket; socket.setdefaulttimeout(1);\ntry: socket.getaddrinfo("example.com", 80); raise SystemExit(1)\nexcept OSError: print("NETWORK_DENIED")'`);
      assert.equal(networkProbe, "NETWORK_DENIED");
      cases.push({ id: "NET-01", status: "PASS", detail: "default network namespace denies external DNS" });
    } else {
      cases.push({ id: "ENV-02", status: "BLOCKED", detail: "python3 is missing from the configured rootfs" });
    }

    await assert.rejects(provider.createSession(bindingA), /already active/);
    const taskA = await firstA.world.backgroundTasks.start({ command: "printf A_TASK; sleep 30", cwd: bindingA.storage.workspace, sessionId: "forged-b" });
    const taskB = await sessionB.world.backgroundTasks.start({ command: "printf B_TASK; sleep 30", cwd: bindingB.storage.workspace, sessionId: bindingB.sessionKey });
    assert.equal(taskA.sessionId, bindingA.sessionKey);
    assert.equal(firstA.world.backgroundTasks.get(taskB.taskId), undefined);
    assert.throws(() => activeA.world.backgroundTasks.getOutput(taskB.taskId, 0), /Unknown taskId/);
    await assert.rejects(firstA.world.backgroundTasks.stop(taskB.taskId), /Unknown taskId/);
    assert.equal(sessionB.world.backgroundTasks.get(taskB.taskId)?.status, "running");
    const foreground = firstA.world.subprocess.executeFile!({ executable: "node", args: ["-e", "setTimeout(()=>{},30000)"], cwd: bindingA.storage.workspace, env: baseEnv(), timeoutMs: 40_000 });
    const foregroundStopped = foreground.then(() => { throw new Error("Stopped foreground unexpectedly succeeded"); }, (error: Error) => assert.match(error.message, /aborted/));
    const detached = await firstA.world.detachedShell.start({ command: "sleep 30 & wait", cwd: bindingA.storage.workspace, env: baseEnv() });
    await firstA.stop("acceptance");
    await foregroundStopped;
    await detached.exit;
    if (detached.pid) assert.throws(() => process.kill(detached.pid!, 0));
    await assert.rejects(firstA.world.shell.execute({ command: "echo OLD", cwd: bindingA.storage.workspace, env: baseEnv(), timeoutMs: 1_000 }), /closed/);
    assert.throws(() => firstA!.world.backgroundTasks.get(taskA.taskId), /closed/);
    assert.equal(await runShell(sessionB, bindingB.storage.workspace, "cat same.txt"), "B_MARKER");
    cases.push({ id: "LIFE-STOP-DRAIN", status: "PASS", detail: "stop rejects old work and reaps foreground/detached workers without stopping B" });
    await firstA.dispose();
    await assert.rejects(provider.createSession(bindingA), /Stale session generation/);
    firstA = undefined;
    resumedA = await provider.createSession(makeBinding(sessionsRoot, "session-a", 2));
    assert.equal(resumedA.world.backgroundTasks.get(taskA.taskId), undefined);
    await assert.rejects(resumedA.world.backgroundTasks.stop(taskA.taskId), /Unknown taskId/);
    cases.push({ id: "EXT-02-OWNER", status: "PASS", detail: "task owner is bound to the trusted session; foreign task reads/stops and old generation task IDs are rejected" });
    assert.equal(await runShell(resumedA, bindingA.storage.workspace, "cat same.txt"), "A_MARKER");
    assert.equal(await runShell(resumedA, bindingA.storage.workspace, "printf '%s' \"$SESSION_ENV\""), "OK");
    if (pythonCommand) {
      assert.equal(await runShell(resumedA, bindingA.storage.workspace, `${pythonCommand} -c 'import pilotdeck_session_marker as m; print(m.VALUE)'`), "A_ONLY");
    }
    if (npmAvailable) {
      assert.equal(await runShell(resumedA, bindingA.storage.workspace, "node -e \"const p=require('/home/agent/.local/npm/node_modules/pilotdeck-session-npm-marker'); process.stdout.write(p.value+':'+p.native)\""), "A_ONLY:A_NATIVE");
      assert.equal(await runShell(resumedA, bindingA.storage.workspace, 'cat "$NPM_CONFIG_USERCONFIG"'), "A_CONFIG");
    }
    cases.push({ id: "LIFE-05", status: "PASS", detail: "generation 2 resumes workspace and environment" });
    capacityProvider = new NsjailSessionExecutionProvider({
      executable,
      rootfs: rootfs!,
      sessionsRoot,
      sandboxMode: "danger-full-access",
      maxActiveSessions: 2,
    });
    capacityHandles.push(
      await capacityProvider.createSession(makeBinding(sessionsRoot, "session-c", 1)),
      await capacityProvider.createSession(makeBinding(sessionsRoot, "session-d", 1)),
    );
    await assert.rejects(capacityProvider.createSession(makeBinding(sessionsRoot, "session-e", 1)), (error: unknown) =>
      error instanceof Error && "code" in error && error.code === "capacity_exceeded");
    cases.push({ id: "LIMIT-02", status: "PASS", detail: "maxActiveSessions returns capacity_exceeded and releases after dispose" });
    await resumedA.dispose();
    resumedA = undefined;
    await provider.deleteSessionStorage(bindingA.sessionKey);
    await assert.rejects(readFile(aPath), { code: "ENOENT" });
    assert.equal(await runShell(sessionB, bindingB.storage.workspace, "cat same.txt"), "B_MARKER");
    const hostObservation = { bMarker: await readFile(bPath, "utf8"), bEntries: await readdir(bindingB.storage.workspace), aDeleted: true,
      bindings: [bindingA, bindingB] };
    for (const entry of cases) entry.hostObservation ??= hostObservation;
    cases.push({ id: "LIFE-RETENTION", status: "PASS", hostObservation, detail: "active storage deletion is rejected; stopped A can be deleted without changing B" });
    const blockedCases = cases.filter((entry) => entry.status === "BLOCKED");
    assert.equal(blockedCases.length, 0, `acceptance prerequisites blocked: ${JSON.stringify(blockedCases)}`);
  } catch (error) {
    failure = error;
    throw error;
  } finally {
    await Promise.allSettled([
      resumedA?.dispose() ?? Promise.resolve(),
      firstA?.dispose() ?? Promise.resolve(),
      sessionB?.dispose() ?? Promise.resolve(),
      ...capacityHandles.map((handle) => handle.dispose()),
      capacityProvider?.dispose() ?? Promise.resolve(),
      provider.dispose(),
    ]);
    if (artifactDir) {
      await mkdir(artifactDir, { recursive: true });
      await writeFile(join(artifactDir, "nsjail-isolation.json"), JSON.stringify({
        status: failure ? "FAIL" : "PASS",
        rootfs,
        executable,
        cases,
        executionObservations,
        failure: failure instanceof Error ? { name: failure.name, message: failure.message } : failure,
      }, null, 2));
    }
    await rm(runRoot, { recursive: true, force: true });
  }
});

test("delegated cgroup v2 enforces session memory/pids and CPU limits", {
  skip: skipReason ?? (!process.env.PILOTDECK_NSJAIL_CGROUP_ROOT ? "delegated PILOTDECK_NSJAIL_CGROUP_ROOT required" : false),
}, async () => {
  const root = await mkdirTemp("pd-limits-");
  const sessionsRoot = join(root, "sessions");
  await mkdir(sessionsRoot);
  const cgroupRoot = process.env.PILOTDECK_NSJAIL_CGROUP_ROOT!;
  const provider = new NsjailSessionExecutionProvider({ executable, rootfs: rootfs!, sessionsRoot, cgroupV2Root: cgroupRoot });
  const bindingA = { ...makeBinding(sessionsRoot, "limits-a", 1), policy: { network: "deny" as const, maxMemoryBytes: 96 * 1024 * 1024, maxPids: 6, maxCpuSeconds: 1 } };
  const bindingB = makeBinding(sessionsRoot, "limits-b", 1);
  const startedAt = new Date().toISOString();
  const keys: string[] = [];
  try {
    const [a, b] = await Promise.all([provider.createSession(bindingA), provider.createSession(bindingB)]);
    keys.push(await nsjailCgroupSessionKey(join(sessionsRoot, bindingA.sandboxKey), bindingA.sandboxKey),
      await nsjailCgroupSessionKey(join(sessionsRoot, bindingB.sandboxKey), bindingB.sandboxKey));
    const memory = await a.world.codeRuntime.run({ executable: "python3", args: ["-c", "x=bytearray(256*1024*1024)"], cwd: bindingA.storage.workspace, env: baseEnv(), timeoutMs: 10_000, stdoutMaxBytes: 4096, stderrMaxBytes: 4096 });
    assert.notEqual(memory.exitCode, 0, JSON.stringify(memory));
    const events = await readFile(join(cgroupRoot, `session-${keys[0]}-1`, "memory.events"), "utf8");
    assert.match(events, /oom_kill [1-9]/, JSON.stringify(memory));
    const pids = await runShell(a, bindingA.storage.workspace, `python3 -c 'import os,time; children=[]
try:
 for i in range(20):
  p=os.fork()
  if p==0: time.sleep(2); os._exit(0)
  children.append(p)
except OSError: print("PIDS_LIMIT")
finally:
 for p in children: os.waitpid(p,0)'`);
    assert.match(pids, /PIDS_LIMIT/);
    const cpu = await a.world.codeRuntime.run({ executable: "python3", args: ["-c", "while True: pass"], cwd: bindingA.storage.workspace, env: baseEnv(), timeoutMs: 10_000, stdoutMaxBytes: 4096, stderrMaxBytes: 4096 });
    assert.notEqual(cpu.exitCode, 0);
    assert.equal(cpu.timedOut, false);
    assert.equal(await runShell(b, bindingB.storage.workspace, "printf B_ALIVE"), "B_ALIVE");
    const artifactDir = process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR;
    if (artifactDir) await writeFile(join(artifactDir, "resource-limits.json"), JSON.stringify({ status: "PASS", memory, memoryEvents: events, pids, cpu, cases: [{ caseId: "LIMIT-MEMORY-PIDS-CPU", status: "PASS", sessionKey: bindingA.sessionKey, sandboxKey: bindingA.sandboxKey, generation: 1,
      request: { memoryBytes: 256 * 1024 * 1024, forkCount: 20, cpu: "while True: pass", policy: bindingA.policy }, response: { memory, pids, cpu },
      hostObservation: { memoryEvents: events, b: "B_ALIVE", cgroupKeys: keys }, startedAt, finishedAt: new Date().toISOString(), exitCode: 0, failureReason: null }] }, null, 2));
  } finally {
    await provider.dispose();
    assert.equal((await readdir(cgroupRoot)).some((name) => keys.some((key) => name.startsWith(`session-${key}-`))), false);
    await rm(root, { recursive: true, force: true });
  }
});

async function runShell(
  handle: Awaited<ReturnType<NsjailSessionExecutionProvider["createSession"]>>,
  cwd: string,
  command: string,
): Promise<string> {
  const result = await handle.world.shell.execute({
    command,
    cwd,
    env: baseEnv(),
    timeoutMs: 20_000,
  });
  assert.equal(result.exitCode, 0, `${command}\n${result.stderr}`);
  return result.stdout.trim();
}

async function expectDenied(
  handle: Awaited<ReturnType<NsjailSessionExecutionProvider["createSession"]>>,
  cwd: string,
  command: string,
  label: string,
): Promise<void> {
  const result = await handle.world.shell.execute({
    command,
    cwd,
    env: baseEnv(),
    timeoutMs: 20_000,
  });
  assert.notEqual(result.exitCode, 0, `${label} unexpectedly succeeded`);
  assert.doesNotMatch(`${result.stdout}\n${result.stderr}`, /A_MARKER|B_MARKER|A_ONLY/);
}

async function findCommand(
  handle: Awaited<ReturnType<NsjailSessionExecutionProvider["createSession"]>>,
  cwd: string,
  command: string,
): Promise<boolean> {
  const result = await handle.world.shell.execute({
    command: `command -v ${shellQuote(command)} >/dev/null 2>&1`,
    cwd,
    env: baseEnv(),
    timeoutMs: 20_000,
  });
  return result.exitCode === 0;
}

async function runCode(
  handle: Awaited<ReturnType<NsjailSessionExecutionProvider["createSession"]>>,
  cwd: string,
  executable: string,
  args: readonly string[],
  signal?: AbortSignal,
) {
  return handle.world.codeRuntime.run({
    executable,
    args,
    cwd,
    env: baseEnv(),
    timeoutMs: 20_000,
    signal,
    stdoutMaxBytes: 64 * 1024,
    stderrMaxBytes: 64 * 1024,
  });
}

function makeBinding(sessionsRoot: string, sandboxKey: string, generation: number): TrustedSessionBinding {
  const root = join(sessionsRoot, sandboxKey);
  return {
    sessionKey: sandboxKey,
    sandboxKey,
    generation,
    storage: {
      workspace: join(root, "workspace"),
      home: join(root, "home"),
      temp: join(root, "tmp"),
    },
    policy: { network: "deny" },
  };
}

function baseEnv(): Record<string, string> {
  return {
    PATH: "/home/agent/.local/bin:/usr/local/bin:/usr/bin:/bin",
    ...(process.env.LD_LIBRARY_PATH ? { LD_LIBRARY_PATH: process.env.LD_LIBRARY_PATH } : {}),
  };
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

async function mkdirTemp(prefix: string): Promise<string> {
  const { mkdtemp } = await import("node:fs/promises");
  return mkdtemp(join(tmpdir(), prefix));
}

function isExecutableAvailable(value: string): boolean {
  try {
    return value.includes("/")
      ? spawnSync(value, ["--help"], { stdio: "ignore", timeout: 2_000 }).status !== null
      : spawnSync("which", [value], { stdio: "ignore", timeout: 2_000 }).status === 0;
  } catch {
    return false;
  }
}
