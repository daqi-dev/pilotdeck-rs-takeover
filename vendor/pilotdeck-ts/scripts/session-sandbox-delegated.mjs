import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { spawn } from "node:child_process";

// Run this entrypoint in a dedicated systemd --user service with Delegate=yes.
const entry = (await readFile("/proc/self/cgroup", "utf8")).split("\n").find((line) => line.startsWith("0::"));
if (!entry || !entry.includes(".service")) throw new Error("Acceptance requires a dedicated delegated cgroup v2 service");
const root = join("/sys/fs/cgroup", entry.slice(3));
const supervisor = join(root, "supervisor");
await mkdir(supervisor, { recursive: true });
await writeFile(join(supervisor, "cgroup.procs"), String(process.pid));
await writeFile(join(root, "cgroup.subtree_control"), "+memory +pids");
const [command = process.execPath, ...supplied] = process.argv.slice(2);
const args = supplied.length ? supplied : ["scripts/session-sandbox-acceptance.mjs"];
const child = spawn(command, args, {
  stdio: "inherit",
  env: { ...process.env, NODE_OPTIONS: undefined, PILOTDECK_NSJAIL_CGROUP_ROOT: root },
});
child.once("error", (error) => { console.error(error); process.exitCode = 2; });
child.once("exit", (code, signal) => { process.exitCode = signal ? 2 : code ?? 2; });
