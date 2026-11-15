import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { McpRuntime } from "../../src/mcp/runtime/McpRuntime.js";
import { NsjailSessionExecutionProvider } from "../../src/sandbox/nsjail/NsjailProvider.js";
import { nextSessionExecutionBinding } from "../../src/sandbox/SessionExecutionStorage.js";

test("session browser MCP isolates persistent profiles, cookies and downloads", {
  skip: process.platform !== "linux" || !process.env.PILOTDECK_NSJAIL_ROOTFS,
  timeout: 180_000,
}, async () => {
  const root = await mkdtemp(join(tmpdir(), "pd-browser-"));
  const sessionsRoot = join(root, "sessions");
  await mkdir(sessionsRoot);
  const provider = new NsjailSessionExecutionProvider({ sessionsRoot,
    executable: process.env.PILOTDECK_NSJAIL_BIN, rootfs: process.env.PILOTDECK_NSJAIL_ROOTFS!,
    cgroupV2Root: process.env.PILOTDECK_NSJAIL_CGROUP_ROOT,
    egressAllowlist: [{ hostname: "example.com", ports: [443] }],
  });
  const cases: unknown[] = [];
  let failure: unknown;
  const probe = async (key: string, generation: number, previous: string | null, network = false) => {
    const startedAt = new Date().toISOString();
    const binding = await nextSessionExecutionBinding(sessionsRoot, key, { network: network ? "allow" : "deny" });
    assert.equal(binding.generation, generation);
    const handle = await provider.createSession(binding);
    await writeFile(join(binding.storage.workspace, "browser.mjs"), BROWSER_MCP);
    const command = await handle.prepareSubprocess!({ executable: "node", args: ["/workspace/browser.mjs"], cwd: binding.storage.workspace, env: { DEBUG: "pw:browser" } });
    const runtime = new McpRuntime([{ id: "browser", transport: "stdio", command: command.executable, args: [...command.args], cwd: command.cwd, env: Object.fromEntries(Object.entries(command.env).filter((entry): entry is [string, string] => entry[1] !== undefined)) }]);
    try {
      const status = await runtime.start();
      assert.equal(status[0]?.status, "ready", JSON.stringify(status));
      const result = await runtime.callTool("browser", "probe", { marker: key, previous, hostRoot: sessionsRoot, network }, { timeoutMs: 60_000 });
      assert.equal(result.isError, undefined, JSON.stringify(result));
      assert.match(JSON.stringify(result), /BROWSER_OK/);
      const downloadRoot = binding.storage.browserDownload!;
      assert.equal(await readFile(join(downloadRoot, "artifact.txt"), "utf8"), key);
      const evidence = { sessionKey: key, sandboxKey: binding.sandboxKey, generation, request: { previous, marker: key, hostRoot: sessionsRoot, network }, response: result,
        hostObservation: { download: key, downloadRoot }, startedAt, finishedAt: new Date().toISOString(), exitCode: 0, failureReason: null };
      cases.push({ caseId: `EXT-08-${key}-${generation}`, status: "PASS", ...evidence });
      if (network) cases.push({ caseId: "EXT-08-EGRESS", status: "PASS", ...evidence });
    } finally { await runtime.stop(); await handle.dispose(); }
  };
  try {
    await Promise.all([probe("browser-a", 1, null), probe("browser-b", 1, null)]);
    await probe("browser-a", 2, "browser-a");
    await probe("browser-c", 1, null, true);
  } catch (error) { failure = error; throw error; }
  finally {
    await provider.dispose();
    const artifacts = process.env.PILOTDECK_ACCEPTANCE_ARTIFACT_DIR;
    if (artifacts) { await mkdir(artifacts, { recursive: true }); await writeFile(join(artifacts, "browser.json"), JSON.stringify({ status: failure ? "FAIL" : "PASS", cases, failure: failure ? String(failure) : undefined }, null, 2)); }
    await rm(root, { recursive: true, force: true });
  }
});

const BROWSER_MCP = `
import { createInterface } from 'node:readline';
import { existsSync, mkdirSync } from 'node:fs';
import { createRequire } from 'node:module';
const { chromium } = createRequire(import.meta.url)('/opt/pilotdeck/playwright-core');
const lines = createInterface({ input: process.stdin });
lines.on('line', async line => {
 const request = JSON.parse(line);
 if (request.id === undefined) return;
 let result;
 try {
  if(request.method === 'initialize') result={ protocolVersion:request.params.protocolVersion,capabilities:{tools:{}},serverInfo:{name:'session-browser',version:'1'} };
  if(request.method === 'tools/list') result={tools:[{name:'probe',description:'Browser isolation probe',inputSchema:{type:'object',properties:{marker:{type:'string'},previous:{type:['string','null']},hostRoot:{type:'string'}}}}]};
  if(request.method === 'tools/call') {
   const { marker, previous, hostRoot, network }=request.params.arguments;
   if(existsSync(hostRoot)) throw new Error('host sessions visible');
   const profile='/home/agent/.pilotdeck/browser/profile';
   const downloads='/home/agent/.pilotdeck/browser/download';
   mkdirSync(profile,{recursive:true});mkdirSync(downloads,{recursive:true});
   const context=await chromium.launchPersistentContext(profile,{executablePath:'/opt/pilotdeck-browser/chrome-headless-shell',headless:true,acceptDownloads:true,downloadsPath:downloads,...(network ? {proxy:{server:process.env.HTTPS_PROXY,bypass:'<-loopback>'}} : {}),args:['--no-sandbox','--disable-dev-shm-usage','--disable-gpu','--in-process-gpu']});
   try {
    const cookies=await context.cookies('https://session.test');
    if((cookies.find(c=>c.name==='marker')?.value ?? null)!==previous) throw new Error('cookie was shared or lost: '+JSON.stringify(cookies));
    await context.addCookies([{name:'marker',value:marker,url:'https://session.test',expires:Math.floor(Date.now()/1000)+86400}]);
    const page=await context.newPage();
    if(network) {
     await page.goto('https://example.com',{timeout:30000});
     if(!(await page.content()).includes('Example Domain')) throw new Error('allowed browser target failed');
     let blocked=false;
     try {
      const denied=await page.goto('http://127.0.0.1:60008/v1/models',{timeout:5000});
      blocked=denied?.status()===403 && (await denied.headerValue('x-proxy-error'))==='blocked-by-allowlist';
     } catch { blocked=true; }
     if(!blocked) throw new Error('browser reached management endpoint');
    }
    await page.setContent('<a download="artifact.txt">download</a>');
    await page.evaluate(marker=>{document.querySelector('a').href=URL.createObjectURL(new Blob([marker],{type:'text/plain'}));},marker);
    const download=page.waitForEvent('download');await page.locator('a').click();await (await download).saveAs(downloads+'/artifact.txt');
    result={content:[{type:'text',text:'BROWSER_OK '+marker}]};
   } finally { await context.close(); }
  }
 } catch(error) { result={isError:true,content:[{type:'text',text:String(error)}]}; }
 process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:request.id,result})+'\\n');
});
lines.on('close',()=>process.exit(0));
`;
