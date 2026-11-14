#!/usr/bin/env node
// ui-smoke · 进程树采样：证明 Rust 一侧的进程树里没有 Node 的 gateway 进程（交付标准第 6 条）。
// 两种用法：
//   包住一条命令：node scripts/ui-smoke/proctree.mjs --prefix <Rust 二进制目录> --out <结果.json> [--interval 50] -- <命令…>
//     起 <命令…>，它运行期间一直采样，它退出后写结果；退出码照传。
//   常驻（全量对拍时后台开着）：node scripts/ui-smoke/proctree.mjs --watch --prefix <目录> --out <结果.json> [--interval 50] [--pidfile F]
//     一直采样，收到 SIGTERM / SIGINT 后写结果并以 0 退出（结论行见下）。不需要事先知道任何进程号。
// 「根」= 本用户的、可执行文件在 --prefix 目录下的进程（Rust 的 pilotdeck server、sidecar 等，谁起的都算；
//   四个槽位并行、server 不断起停也照样逐个认出来）。
// 跟踪集合 = 根 + 根的后代 + 与某个根同一会话的进程。一个进程一旦被认作后代就一直算（父进程先退、它被 init 收养也不丢）。
// 基线 gateway（node dist/src/cli/pilotdeck.js server）由对拍采集器直接起，不是任何 Rust 进程的后代，不计入。
// 开销：/proc 的条目按 (进程号, 启动时刻) 缓存，每轮只给新出现的进程读 stat 和 exe，
//   出现不到 2 秒的进程（可能刚 fork 还没 exec）每轮重读 exe；命令行只读跟踪集合里的。结果里记采样器自己的 CPU 时间。
// 输出：<结果.json>（汇总）与同名 .txt（人读清单）。两者都有一条结论行，stdout 也打印：
//   PROCTREE_RESULT {"nodeGatewayUnderRust":<个数>,"ok":<true|false>,...}
// 判 Node gateway 的规则：命令行里出现基线 gateway 的入口（dist/src/cli/pilotdeck.js、src/cli/pilotdeck.ts、
//   dist/src/gateway/、src/gateway/server/），或 node 进程的参数里有独立的 server 词。
// 命令行按「种类」汇总时做路径归一（UUID、bbx 槽位目录、mkdtemp 随机后缀、端口、长数字），每种留一条原样例子。
// 命令行里形如 key=… / token=… / sk-… 的值抹掉，环境变量不记。Linux 读 /proc；其他平台退到 ps。
import fs from 'node:fs';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';

const argv = process.argv.slice(2);
const dash = argv.indexOf('--');
const head = dash < 0 ? argv : argv.slice(0, dash);
const opts = {};
for (let i = 0; i < head.length; i++) {
  const k = head[i].replace(/^--/, '');
  if (k === 'watch') opts.watch = true; else opts[k] = head[++i];
}
const cmd = dash < 0 ? [] : argv.slice(dash + 1);
if (!opts.prefix || !opts.out || (!opts.watch && !cmd.length) || (opts.watch && cmd.length)) {
  console.error('usage: proctree.mjs --prefix DIR --out FILE.json [--interval MS] -- CMD...\n       proctree.mjs --watch --prefix DIR --out FILE.json [--interval MS] [--pidfile FILE]');
  process.exit(2);
}
const PREFIX = fs.realpathSync(path.resolve(opts.prefix)) + path.sep;
const INTERVAL = Number(opts.interval ?? 50);
const OUT = path.resolve(opts.out);
const LINUX = process.platform === 'linux';
const SELF = process.pid;

const redact = (s) => s
  .replace(/((?:api[_-]?key|token|secret|password|authorization)[=:]\s*)\S+/gi, '$1<redacted>')
  .replace(/\bsk-[A-Za-z0-9_-]{8,}/g, 'sk-<redacted>');
const normalize = (s) => s
  .replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, '<uuid>')
  .replace(/pdrs-bb[0-9a-z]\b/g, 'pdrs-bb<slot>')
  .replace(/([_-])[A-Za-z0-9]{6}(?=\/|\s|$)/g, (m, sep) => (/[0-9]/.test(m) || /[A-Z]/.test(m) ? `${sep}<rand>` : m))
  .replace(/(--port[= ])\d+/g, '$1<port>')
  .replace(/\b(127\.0\.0\.1|localhost):\d+/g, '$1:<port>')
  .replace(/\b\d{5,}\b/g, '<n>');

// ── 进程表 ──
const cache = new Map(); // pid → { start, ppid, sid, exe, seenAt }
function readProc(pid, now) {
  const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
  const rest = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
  let exe = null; try { exe = fs.readlinkSync(`/proc/${pid}/exe`); } catch {}
  return { pid, ppid: Number(rest[1]), sid: Number(rest[3]), start: rest[19], exe, seenAt: now };
}
function refreshLinux(now) {
  const alive = new Set();
  for (const name of fs.readdirSync('/proc')) {
    if (name.charCodeAt(0) > 57) continue; // 非数字条目
    const pid = Number(name); if (!Number.isInteger(pid)) continue;
    alive.add(pid);
    const c = cache.get(pid);
    if (!c) { try { cache.set(pid, readProc(pid, now)); } catch {} continue; }
    if (now - c.seenAt < 2000) { try { c.exe = fs.readlinkSync(`/proc/${pid}/exe`); } catch {} }
  }
  for (const pid of cache.keys()) if (!alive.has(pid)) cache.delete(pid);
}
function refreshPs(now) {
  const r = spawnSync('ps', ['-axo', 'pid=,ppid=,sess=,lstart=,comm='], { encoding: 'utf8' });
  const alive = new Set();
  for (const line of r.stdout.split('\n')) {
    const m = line.trim().match(/^(\d+)\s+(\d+)\s+(\S+)\s+(\S+\s+\S+\s+\S+\s+\S+\s+\S+)\s+(.*)$/); if (!m) continue;
    const pid = Number(m[1]); alive.add(pid);
    const c = cache.get(pid);
    if (!c || c.start !== m[4]) cache.set(pid, { pid, ppid: Number(m[2]), sid: m[3], start: m[4], exe: m[5], seenAt: now });
    else c.exe = m[5];
  }
  for (const pid of cache.keys()) if (!alive.has(pid)) cache.delete(pid);
}
const cmdlineOf = (pid) => {
  if (LINUX) { try { return fs.readFileSync(`/proc/${pid}/cmdline`, 'utf8').replace(/\0+$/, '').split('\0').join(' '); } catch { return null; } }
  const r = spawnSync('ps', ['-o', 'args=', '-p', String(pid)], { encoding: 'utf8' }); return r.stdout.trim() || null;
};

// ── 采样 ──
const sticky = new Map(); // `${pid}/${start}` → 关系（root / descendant / same-session）
const seen = new Map();   // `${pid}/${start}/${cmdline}` → 记录
const rootsSeen = new Set();
let ticks = 0; let maxTickMs = 0; let maxGapMs = 0; let lastTick = 0; let maxTracked = 0;
const t0 = Date.now(); const cpu0 = process.cpuUsage();
function tick() {
  const now = Date.now();
  if (lastTick) maxGapMs = Math.max(maxGapMs, now - lastTick);
  lastTick = now;
  if (LINUX) refreshLinux(now); else refreshPs(now);
  const children = new Map();
  for (const p of cache.values()) { if (!children.has(p.ppid)) children.set(p.ppid, []); children.get(p.ppid).push(p); }
  const tracked = new Map();
  const mark = (p, rel) => { const k = `${p.pid}/${p.start}`; if (!sticky.has(k)) sticky.set(k, rel); tracked.set(p.pid, sticky.get(k)); };
  const walk = (p, rel) => {
    if (tracked.has(p.pid) || p.pid === SELF) return;
    mark(p, rel);
    for (const c of children.get(p.pid) ?? []) walk(c, 'descendant');
  };
  const roots = [];
  for (const p of cache.values()) if (p.exe && p.exe.startsWith(PREFIX)) roots.push(p);
  for (const r of roots) { walk(r, 'root'); rootsSeen.add(`${r.pid}/${r.start}`); }
  // 以前认作后代、父进程已退出的（被收养），以及它们的后代
  for (const p of cache.values()) if (!tracked.has(p.pid) && sticky.has(`${p.pid}/${p.start}`)) walk(p, sticky.get(`${p.pid}/${p.start}`));
  const rootSids = new Set(roots.filter((r) => String(r.sid) === String(r.pid)).map((r) => String(r.sid)));
  if (rootSids.size) for (const p of cache.values()) if (!tracked.has(p.pid) && rootSids.has(String(p.sid))) walk(p, 'same-session');
  maxTracked = Math.max(maxTracked, tracked.size);
  for (const [pid, rel] of tracked) {
    const p = cache.get(pid);
    const cl = cmdlineOf(pid);
    if (cl === null || cl === '') continue; // 已退出（僵尸）或内核线程
    const k = `${pid}/${p.start}/${cl}`;
    const rec = seen.get(k) ?? { pid, cmdline: redact(cl), exe: p.exe, rel, samples: 0 };
    rec.samples++; seen.set(k, rec);
  }
  ticks++;
  maxTickMs = Math.max(maxTickMs, Date.now() - now);
}

const NODE_GATEWAY = /(dist\/src\/cli\/pilotdeck\.js|src\/cli\/pilotdeck\.ts|dist\/src\/gateway\/|src\/gateway\/server\/)/;
const isNode = (r) => /(^|\/)node(\d+)?( \(deleted\))?$/.test(r.exe ?? '') || /^(\S*\/)?node\s/.test(r.cmdline);
function summarize(exitCode) {
  const kinds = new Map();
  for (const r of seen.values()) {
    const key = normalize(r.cmdline);
    const node = isNode(r);
    const k = kinds.get(key) ?? { cmdline: key, example: r.cmdline, exe: r.exe, rel: new Set(), processes: 0, samples: 0, node, nodeGateway: NODE_GATEWAY.test(r.cmdline) || (node && /\sserver(\s|$)/.test(r.cmdline)) };
    k.processes++; k.samples += r.samples; k.rel.add(r.rel); kinds.set(key, k);
  }
  const list = [...kinds.values()].map((k) => ({ ...k, rel: [...k.rel].sort() })).sort((a, b) => b.processes - a.processes || a.cmdline.localeCompare(b.cmdline));
  const durationMs = Date.now() - t0;
  const cpu = process.cpuUsage(cpu0); const cpuMs = Math.round((cpu.user + cpu.system) / 1000);
  const nodeGatewayUnderRust = list.filter((k) => k.nodeGateway).reduce((n, k) => n + k.processes, 0);
  const result = {
    nodeGatewayUnderRust, ok: nodeGatewayUnderRust === 0 && ticks > 0, rustRoots: rootsSeen.size, processes: seen.size, kinds: list.length,
    ticks, intervalMs: INTERVAL, maxGapMs, durationMs, samplerCpuPercent: durationMs ? Math.round((cpuMs / durationMs) * 1000) / 10 : 0,
  };
  const summary = {
    mode: opts.watch ? 'watch' : 'wrap', command: cmd.join(' ') || null, prefix: PREFIX, platform: process.platform,
    startedAt: new Date(t0).toISOString(), exitCode, maxTickMs, maxTracked, samplerCpuMs: cpuMs, result,
    nodeProcesses: list.filter((k) => k.node).reduce((n, k) => n + k.processes, 0), list,
  };
  fs.mkdirSync(path.dirname(OUT), { recursive: true });
  fs.writeFileSync(OUT, JSON.stringify(summary, null, 2));
  const resultLine = `PROCTREE_RESULT ${JSON.stringify(result)}`;
  const lines = [
    `# proctree (${summary.mode}): ${summary.command ?? '(watch)'}`,
    `# prefix=${PREFIX} interval=${INTERVAL}ms duration=${durationMs}ms ticks=${ticks} maxTick=${maxTickMs}ms maxGap=${maxGapMs}ms sampler-cpu=${cpuMs}ms (${result.samplerCpuPercent}% of one core) exit=${exitCode}`,
    `# rust-roots=${rootsSeen.size} processes=${seen.size} kinds=${list.length} node=${summary.nodeProcesses} node-gateway=${nodeGatewayUnderRust}`,
    resultLine,
    '# 进程数\t采样次数\t关系\tnode\t命令行（归一后）',
    ...list.map((k) => `${k.processes}\t${k.samples}\t${k.rel.join(',')}\t${k.nodeGateway ? 'NODE-GATEWAY' : k.node ? 'node' : '-'}\t${k.cmdline}`),
  ];
  fs.writeFileSync(OUT.replace(/\.json$/, '') + '.txt', lines.join('\n') + '\n');
  console.log(resultLine);
  console.error(`[proctree] ${ticks} ticks over ${durationMs}ms (max gap ${maxGapMs}ms, cpu ${result.samplerCpuPercent}%), ${rootsSeen.size} rust roots, ${seen.size} processes, ${list.length} kinds, node-gateway=${nodeGatewayUnderRust} → ${OUT}`);
}

let busy = false;
const timer = setInterval(() => { if (busy) return; busy = true; try { tick(); } finally { busy = false; } }, INTERVAL);
if (opts.watch) {
  if (opts.pidfile) fs.writeFileSync(opts.pidfile, `${process.pid}\n`);
  const finish = () => { clearInterval(timer); tick(); summarize(null); if (opts.pidfile) { try { fs.rmSync(opts.pidfile); } catch {} } process.exit(0); };
  process.on('SIGTERM', finish); process.on('SIGINT', finish);
} else {
  const child = spawn(cmd[0], cmd.slice(1), { stdio: 'inherit' });
  for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => child.kill(sig));
  child.on('exit', (code, sig) => { clearInterval(timer); tick(); summarize(code ?? sig); process.exit(code ?? 1); });
}
