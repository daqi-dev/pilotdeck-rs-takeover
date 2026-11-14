#!/usr/bin/env node
// ui-smoke · Linux 上替代 sandbox-exec 的事后检查：读 run.mjs 在 UI_SMOKE_AUDIT=strace 下记的 <前缀>.{gateway,ui}.strace，
// 列出三类越界：连 localhost 以外的网络地址、写根目录与 /dev 以外的路径、读真实 ~/.pilotdeck。
// 用法：node scripts/ui-smoke/audit.mjs <输出目录> <前缀> <根目录> [真实 ~/.pilotdeck]
// 连接看全部尝试（含非阻塞的 EINPROGRESS），其余只看成功的调用。*at 调用的相对路径按 strace -y 给出的目录 fd 还原；AT_FDCWD 的相对路径无法还原 cwd，列为「待看」（不计越界）。
// 退出码：0 没有越界；1 有。
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const [outDir, label, rootArg, realArg] = process.argv.slice(2);
if (!outDir || !label || !rootArg) { console.error('usage: audit.mjs <outdir> <label> <root> [real ~/.pilotdeck]'); process.exit(2); }
const ROOTS = [path.resolve(rootArg)]; try { ROOTS.push(fs.realpathSync(rootArg)); } catch {}
const REAL = path.resolve(realArg ?? path.join(os.userInfo().homedir, '.pilotdeck'));
const under = (p, d) => p === d || p.startsWith(d + '/');
// 写也允许落在 /dev（与 macOS 沙箱配置相同）和 /proc/self、/proc/<pid>（运行时写自己的 oom_score_adj 之类）。
const writeOk = (p) => ROOTS.some((r) => under(p, r)) || under(p, '/dev') || /^\/proc\/(self|\d+)\//.test(p);

const WRITE_FLAGS = /O_WRONLY|O_RDWR|O_CREAT|O_TRUNC|O_APPEND/;
let bad = 0; const report = [];
// gateway 中途重启时有 gateway2… 各一份；gateway 与 ui 至少各要有一份。
const names = fs.readdirSync(outDir).filter((f) => f.startsWith(`${label}.`) && f.endsWith('.strace')).map((f) => f.slice(label.length + 1, -'.strace'.length)).sort();
for (const need of ['gateway', 'ui']) if (!names.includes(need)) { report.push(`${need}: no strace file`); bad++; }
for (const name of names) {
  const f = path.join(outDir, `${label}.${name}.strace`);
  const counts = { lines: 0, connect: 0, writes: 0 };
  const issues = new Map(); const note = (k) => issues.set(k, (issues.get(k) ?? 0) + 1);
  for (const line of fs.readFileSync(f, 'utf8').split('\n')) {
    if (!line) continue; counts.lines++;
    // 完整的一行「pid sc(args) = ret …」，或 -f 下被打断的「pid sc(args <unfinished ...>」（结果未知，按尝试过算）。
    const m = line.match(/^\d+\s+(\w+)\((.*)\)\s+=\s+(-?\d+|\?)(.*)$/) ?? line.match(/^\d+\s+(\w+)\((.*?)\s*<unfinished \.\.\.>$/);
    if (!m) continue;
    const [, sc, args, ret, tail] = m;
    // 连接：非阻塞套接字返回 -1 EINPROGRESS 也是真的连出去了，所以只要尝试过就检查；
    // 其余调用：失败的（-1 加 errno）不算，只看成功或结果未知的。
    if (sc !== 'connect' && ret === '-1') continue;
    if (sc === 'connect' && ret === '-1' && !/EINPROGRESS/.test(tail ?? '')) continue;
    if (sc === 'connect') {
      counts.connect++;
      if (/sa_family=AF_UNIX/.test(args)) continue;
      const v4 = args.match(/sin_addr=inet_addr\("([^"]+)"\)/); const v6 = args.match(/inet_pton\(AF_INET6, "([^"]+)"/);
      const addr = v4?.[1] ?? v6?.[1];
      if (addr === undefined) { if (!/AF_NETLINK/.test(args)) note(`connect ?: ${args.slice(0, 160)}`); continue; }
      const port = args.match(/sin6?_port=htons\((\d+)\)/)?.[1];
      // AF_UNSPEC 断开、0.0.0.0 都不是向外连接
      if (/^127\./.test(addr) || addr === '::1' || addr === '0.0.0.0' || addr === '::' || /sa_family=AF_UNSPEC/.test(args)) continue;
      note(`connect ${addr}:${port}`);
      continue;
    }
    // strace -y 把目录 fd 写成 5</abs/dir>：*at 调用的相对路径按它还原（AT_FDCWD 的相对路径仍无法还原）。
    const dirfd = args.match(/^\d+<([^>]*)>,\s*"/)?.[1];
    const paths = [...args.matchAll(/"((?:[^"\\]|\\.)*)"/g)].map((x) => (dirfd && !path.isAbsolute(x[1]) ? path.join(dirfd, x[1]) : x[1]));
    const isOpen = sc === 'open' || sc === 'openat' || sc === 'creat';
    if (isOpen && !(sc === 'creat' || WRITE_FLAGS.test(args))) {
      // 只读打开：只查是否读了真实 ~/.pilotdeck
      if (paths.some((p) => path.isAbsolute(p) && under(p, REAL))) note(`read real ~/.pilotdeck: ${paths[0]}`);
      continue;
    }
    counts.writes++;
    // 写类调用：rename/link 的目标、unlink、mkdir、chmod、truncate、写方式 open
    for (const p of paths) {
      if (!path.isAbsolute(p)) { note(`relative path (check by hand): ${sc} ${p}`); continue; }
      if (under(p, REAL)) note(`write real ~/.pilotdeck: ${sc} ${p}`);
      else if (!writeOk(p)) note(`write outside root: ${sc} ${p}`);
    }
  }
  report.push(`${name}: ${counts.lines} syscalls, ${counts.connect} connect, ${counts.writes} write-like; ${issues.size ? `${issues.size} finding(s)` : 'clean'}`);
  for (const [k, n] of [...issues].sort()) { report.push(`  ${n}× ${k}`); if (!k.startsWith('relative path')) bad++; }
}
report.push(`越界 ${bad} 处`);
console.log(report.join('\n'));
process.exit(bad ? 1 : 0);
