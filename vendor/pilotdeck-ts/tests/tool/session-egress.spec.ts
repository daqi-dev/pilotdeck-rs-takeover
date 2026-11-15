import assert from "node:assert/strict";
import test from "node:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createSessionEgressProxy, SessionEgressPolicy, isPublicAddress } from "../../src/sandbox/egress/SessionEgressProxy.js";
import { createSessionMcpHttpMock } from "../fixtures/session-mcp-http-mock.js";
import { NsjailSessionExecutionProvider } from "../../src/sandbox/nsjail/NsjailProvider.js";
import type { TrustedSessionBinding } from "../../src/sandbox/SessionExecutionProvider.js";
import { HttpHookExecutor } from "../../src/extension/hooks/execution/HttpHookExecutor.js";
import { McpClient } from "../../src/mcp/client/McpClient.js";

test("session egress rejects private, mapped, host and unlisted destinations", async () => {
  for (const address of ["0.0.0.0", "127.0.0.1", "10.0.0.1", "169.254.169.254", "172.16.0.1", "192.168.1.1", "100.64.0.1", "198.18.0.1", "::1", "::ffff:127.0.0.1", "fc00::1", "fe80::1", "2001:db8::1"]) {
    assert.equal(isPublicAddress(address), false, address);
  }
  assert.equal(isPublicAddress("1.1.1.1"), true);
  assert.equal(isPublicAddress("2606:4700:4700::1111"), true);
  const policy = new SessionEgressPolicy([{ hostname: "127.0.0.1", ports: [80] }]);
  await assert.rejects(policy.resolve(new URL("http://127.0.0.1")), /private/);
  await assert.rejects(policy.resolve(new URL("https://example.com")), /allowlist/);
  await assert.rejects(policy.resolve(new URL("http://127.0.0.1:60008")), /allowlist/);
  await assert.rejects(policy.resolve(new URL("file:///etc/passwd")), /Unsupported/);
});

test("local service capabilities validate exact origins and sockets without opening private networks", async () => {
  const root = await mkdtemp("/tmp/pd-mcp-policy-");
  const mock = await createSessionMcpHttpMock(join(root, "mock.sock"), "unit");
  const controller = new AbortController();
  const options = { directory: join(root, "proxy"), rules: [], signal: controller.signal };
  let proxy: Awaited<ReturnType<typeof createSessionEgressProxy>> | undefined;
  try {
    for (const origin of ["http://127.0.0.1", "https://mock.invalid", "http://mock.invalid/path", "http://mock.invalid?x=1"]) {
      await assert.rejects(createSessionEgressProxy({ ...options, localServices: [{ origin, socketPath: mock.socketPath }] }), /Invalid/);
    }
    await writeFile(join(root, "file"), "not a socket");
    await assert.rejects(createSessionEgressProxy({ ...options, localServices: [{ origin: "http://mock.invalid", socketPath: join(root, "file") }] }), /Unix socket/);
    const service = { origin: "http://mock.invalid", socketPath: mock.socketPath };
    await assert.rejects(createSessionEgressProxy({ ...options, localServices: [service, service] }), /Invalid/);
    proxy = await createSessionEgressProxy({ ...options, localServices: [service] });
    service.socketPath = "/not-the-registered-socket";
    for (const transport of ["streamable_http", "sse"] as const) {
      const client: McpClient = new McpClient({ id: `unit-${transport}`, transport, url: `http://mock.invalid/${transport === "sse" ? "sse" : "mcp"}` }, { fetch: proxy.network.fetch });
      try {
        await client.start();
        assert.equal((await client.listTools())[0]?.toolName, "session_marker");
        assert.deepEqual((await client.callTool("session_marker", {})).content, [{ type: "text", text: "MCP_MARKER_unit" }]);
      } finally { await client.close(); }
    }
    await assert.rejects(proxy.network.fetch("http://other.invalid/mcp"), /allowlist/);
    await assert.rejects(proxy.network.fetch("http://user:pass@mock.invalid/mcp"), /Unsupported/);
    await assert.rejects(proxy.network.fetch("http://mock.invalid/redirect?to=http%3A%2F%2F127.0.0.1%3A60008"), /allowlist/);
  } finally {
    await proxy?.dispose();
    await mock.dispose();
    await rm(root, { recursive: true, force: true });
  }
});

test("real nsjail uses a session Unix proxy while raw sockets and other proxies remain inaccessible", {
  skip: process.platform !== "linux" || !process.env.PILOTDECK_NSJAIL_ROOTFS,
  timeout: 120_000,
}, async () => {
  const root = await mkdtemp(join(tmpdir(), "pd-egress-"));
  const observations: unknown[] = [];
  const cases: unknown[] = [];
  const startedAt = new Date().toISOString();
  const sessionsRoot = join(root, "sessions");
  const mocks = new Map<string, Awaited<ReturnType<typeof createSessionMcpHttpMock>>>();
  for (const owner of ["a", "b"]) mocks.set(owner, await createSessionMcpHttpMock(join(root, `mcp-${owner}.sock`), owner));
  await mkdir(sessionsRoot);
  const provider = new NsjailSessionExecutionProvider({
    rootfs: process.env.PILOTDECK_NSJAIL_ROOTFS!, executable: process.env.PILOTDECK_NSJAIL_BIN,
    sessionsRoot, cgroupV2Root: process.env.PILOTDECK_NSJAIL_CGROUP_ROOT,
    egressAllowlist: [{ hostname: "example.com", ports: [80, 443] }, { hostname: "httpbin.org", ports: [443] }, { hostname: "mcp.deepwiki.com", ports: [443] },
      { hostname: "127.0.0.1", ports: [60008] }],
    egressLocalServices: (binding) => [{ origin: `http://mcp-${binding.sessionKey}.invalid`, socketPath: mocks.get(binding.sessionKey)!.socketPath }],
    onEgress: (binding, observation) => observations.push({ ...binding, ...observation }),
  });
  const binding = (name: string): TrustedSessionBinding => ({
    sessionKey: name, sandboxKey: name, generation: 1,
    storage: { workspace: join(sessionsRoot, name, "workspace"), home: join(sessionsRoot, name, "home"), temp: join(sessionsRoot, name, "tmp") },
    policy: { network: "allow" },
  });
  let failure: unknown;
  try {
    const [a, b] = await Promise.all([provider.createSession(binding("a")), provider.createSession(binding("b"))]);
    const response = await a.world.network!.fetch("https://example.com");
    assert.equal(response.status, 200);
    assert.match(await response.text(), /Example Domain/i);
    await assert.rejects(a.world.network!.fetch("https://example.org"), /allowlist/);
    await assert.rejects(a.world.network!.fetch("http://127.0.0.1:60008/v1/models"), /private/);
    const redirect = (target: string) => `https://httpbin.org/redirect-to?url=${encodeURIComponent(target)}`;
    const redirected = await a.world.network!.fetch(redirect("https://example.com"));
    assert.equal(redirected.status, 200);
    await redirected.body?.cancel();
    await assert.rejects(a.world.network!.fetch(redirect("http://127.0.0.1:60008/v1/models")), /private/);
    await assert.rejects(a.world.network!.fetch(redirect("https://example.org"), { redirect: "manual" }), /allowlist/);
    const requestResponse = await a.world.network!.fetch(new Request("https://httpbin.org/post", { method: "POST", body: "SESSION_REQUEST" }));
    assert.equal(requestResponse.status, 200);
    assert.match(await requestResponse.text(), /SESSION_REQUEST/);
    const hook = await new HttpHookExecutor(a.world.network!.fetch).execute({
      hook: { type: "http", url: "http://127.0.0.1:60008/v1/models" },
      hookInput: { hookEventName: "PreToolUse", sessionId: "a", cwd: "/workspace", transcriptPath: "/tmp/session.jsonl", permissionMode: "bypassPermissions", toolName: "bash", toolInput: {} },
    });
    assert.equal(hook.outcome, "non_blocking_error");
    assert.match(hook.stderr, /private/);
    const python = `import os, socket, urllib.request
assert not os.path.exists(${JSON.stringify(join(sessionsRoot, "b", ".pilotdeck", "control", "egress", "proxy.sock"))})
try:
 socket.create_connection(('1.1.1.1', 443), timeout=1)
 raise AssertionError('raw network escaped')
except OSError: pass
response = urllib.request.urlopen('http://example.com', timeout=30)
assert b'Example Domain' in response.read()
print('SESSION_PROXY_OK')
`;
    const result = await a.world.subprocess.executeFile!({ executable: "python3", args: ["-c", python], cwd: binding("a").storage.workspace, env: process.env, timeoutMs: 60_000 });
    assert.equal(result.exitCode, 0, result.stderr);
    assert.match(result.stdout, /SESSION_PROXY_OK/);
    const bResult = await b.world.shell.execute({ command: "printf B_ALIVE", cwd: binding("b").storage.workspace, env: process.env, timeoutMs: 10_000 });
    assert.equal(bResult.stdout, "B_ALIVE");
    await Promise.all(["a", "b"].map(async (owner) => {
      const handle = owner === "a" ? a : b;
      const other = owner === "a" ? "b" : "a";
      const url = `http://mcp-${owner}.invalid`;
      await assert.rejects(handle.world.network!.fetch(`http://mcp-${other}.invalid/mcp`), /allowlist/);
      await assert.rejects(handle.world.network!.fetch(`${url}:81/mcp`), /allowlist/);
      await assert.rejects(handle.world.network!.fetch(`${url}/redirect?to=${encodeURIComponent(`http://mcp-${other}.invalid/mcp`)}`), /allowlist/);
      await assert.rejects(handle.world.network!.fetch(`${url}/redirect?to=http%3A%2F%2F127.0.0.1%3A60008`), /private/);
      const script = `import os,json,urllib.request
assert not os.path.exists(${JSON.stringify(mocks.get(other)!.socketPath)})
body=json.dumps({'jsonrpc':'2.0','id':1,'method':'tools/call','params':{'name':'session_marker','arguments':{}}}).encode()
request=urllib.request.Request('${url}/mcp',data=body,headers={'Content-Type':'application/json','Accept':'application/json, text/event-stream'})
result=json.load(urllib.request.urlopen(request,timeout=5))
assert result['result']['content'][0]['text']=='MCP_MARKER_${owner}'
try:
 urllib.request.urlopen('http://mcp-${other}.invalid/mcp',timeout=5)
 raise AssertionError('other session mock reached')
except urllib.error.HTTPError as error: assert error.code==403
print('GUEST_MCP_${owner}_OK')
`;
      const guest = await handle.world.subprocess.executeFile!({ executable: "python3", args: ["-c", script], cwd: binding(owner).storage.workspace, env: {}, timeoutMs: 10_000 });
      assert.equal(guest.exitCode, 0, guest.stderr);
      assert.match(guest.stdout, new RegExp(`GUEST_MCP_${owner}_OK`));
      for (const transport of ["streamable_http", "sse"] as const) {
        const client = new McpClient({ id: `${owner}-${transport}`, transport, url: `${url}/${transport === "sse" ? "sse" : "mcp"}` }, { fetch: handle.world.network!.fetch });
        try {
          await client.start();
          assert.equal((await client.listTools())[0]?.toolName, "session_marker");
          assert.deepEqual((await client.callTool("session_marker", { sessionKey: other })).content, [{ type: "text", text: `MCP_MARKER_${owner}` }]);
        } finally { await client.close(); }
      }
    }));
    cases.push({ id: "EXT-03-LOCAL-OWNERSHIP", status: "PASS", response: [...mocks].map(([owner, mock]) => ({ owner, requests: mock.requests })), detail: "A/B HTTP and SSE tool calls return only their own marker; forged owner, other origin/socket, wrong port and private/cross-session redirect denied" });
    for (const fixture of [{ transport: "streamable_http", url: "https://mcp.deepwiki.com/mcp", id: "EXT-03-public-http" },
      { transport: "streamable_http", url: "http://mcp-a.invalid/mcp", id: "EXT-03-streamable_http" },
      { transport: "sse", url: "http://mcp-a.invalid/sse", id: "EXT-03-sse" }] as const) {
      const { transport, url } = fixture;
      const client = new McpClient({ id: `controlled-${transport}`, transport, url }, { fetch: a.world.network!.fetch, handshakeTimeoutMs: 20_000 });
      const mcpStartedAt = new Date().toISOString();
      try {
        await client.start();
        assert.equal(client.getStatus(), "ready");
        const tools = await client.listTools();
        assert.ok(tools.length > 0);
        const call = url.startsWith("http://mcp-a.invalid") ? await client.callTool("session_marker", {}) : null;
        if (call) assert.deepEqual(call.content, [{ type: "text", text: "MCP_MARKER_a" }]);
        cases.push({ id: fixture.id, status: "PASS", sessionKey: "a", sandboxKey: "a", generation: 1,
          request: { transport, url, method: call ? "initialize/tools/list/tools/call" : "initialize/tools/list" }, response: { tools, call },
          hostObservation: { proxy: observations.filter((entry) => (entry as { target: string }).target === new URL(url).origin), mock: mocks.get("a")!.requests },
          startedAt: mcpStartedAt, finishedAt: new Date().toISOString(), exitCode: 0, failureReason: null });
      } catch (error) {
        if (error instanceof assert.AssertionError) throw error;
        cases.push({ id: fixture.id, status: url.startsWith("http://mcp-a.invalid") ? "FAIL" : "BLOCKED", sessionKey: "a", sandboxKey: "a", generation: 1,
          request: { transport, url, method: "initialize/tools/list" }, response: { error: String(error) },
          startedAt: mcpStartedAt, finishedAt: new Date().toISOString(), exitCode: null,
          hostObservation: observations.filter((entry) => (entry as { target: string }).target === new URL(url).origin),
          failureReason: `MCP fixture did not complete the handshake/tool call: ${String(error)}` });
      } finally { await client.close(); }
      const denied = new McpClient({ id: `denied-${transport}`, transport, url: "http://127.0.0.1:60008/v1/models" }, { fetch: a.world.network!.fetch, handshakeTimeoutMs: 2_000 });
      try { await assert.rejects(denied.start()); } finally { await denied.close(); }
    }
    for (let i = 0; i < 2; i++) {
      const repeated = await a.world.shell.execute({ command: "python3 -c \"import urllib.request; assert b'Example Domain' in urllib.request.urlopen('http://example.com',timeout=20).read(); print('SHELL_PROXY_OK')\"", cwd: binding("a").storage.workspace, env: {}, timeoutMs: 30_000 });
      assert.equal(repeated.exitCode, 0, repeated.stderr);
      assert.match(repeated.stdout, /SHELL_PROXY_OK/);
    }
    await a.dispose();
    await assert.rejects(a.world.network!.fetch("https://example.com"));
    cases.push({ id: "EXT-09", status: "PASS", detail: "host adapter and jailed Python use allowlisted, DNS-pinned session proxy; private, unknown targets and raw sockets denied" });
    cases.push({ id: "EXT-05-HTTP", status: "PASS", detail: "HTTP hook cannot bypass the session egress proxy to reach the host management API" });
    cases.push({ id: "NET-REDIRECT", status: "PASS", detail: "allowed cross-origin redirects work; private and unlisted redirects are rejected even in manual mode" });
    cases.push({ id: "NET-PROXY-ENV", status: "PASS", detail: "repeated shell invocations use the current namespace relay rather than a stale persisted proxy port" });
  } catch (error) { failure = error; throw error; }
  finally {
    await provider.dispose();
    const artifactDir = process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR;
    if (artifactDir) {
      await mkdir(artifactDir, { recursive: true });
      const status = failure || cases.some((entry) => (entry as { status: string }).status === "FAIL") ? "FAIL" : cases.some((entry) => (entry as { status: string }).status === "BLOCKED") ? "BLOCKED" : "PASS";
      await writeFile(join(artifactDir, "egress.json"), JSON.stringify({ status, cases: cases.map((entry) => ({ sessionKey: "a", sandboxKey: "a", generation: 1,
        startedAt, finishedAt: new Date().toISOString(), request: { operation: "session proxy allow/deny/redirect/shell/MCP" }, response: observations,
        hostObservation: observations, exitCode: failure ? 1 : 0, failureReason: failure ? String(failure) : null, ...entry as object })), observations, failure: failure ? String(failure) : undefined }, null, 2));
    }
    await Promise.all([...mocks.values()].map((mock) => mock.dispose()));
    await rm(root, { recursive: true, force: true });
  }
});
