import { cp, mkdir, rm, access, writeFile } from "node:fs/promises";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const [base, destination, image = "wildclawbench-ubuntu:v1.3-tavily-pipfix-browser"] = process.argv.slice(2);
if (!base || !destination || resolve(base) === resolve(destination)) throw new Error("Provide distinct base and new rootfs directories");
try { await access(destination); throw new Error("Rootfs destination already exists"); }
catch (error) { if (error.code !== "ENOENT") throw error; }
await cp(base, destination, { recursive: true, dereference: false, verbatimSymlinks: true });
const command = (program, args) => {
  const result = spawnSync(program, args, { encoding: "utf8", timeout: 120_000 });
  if (result.status !== 0) throw new Error(`${program} failed: ${result.stderr}`);
  return result.stdout.trim();
};
const container = command("docker", ["create", "--network", "none", image]);
try {
  const shell = (script) => command("docker", ["run", "--rm", "--network", "none", "--entrypoint", "/bin/sh", image, "-c", script]);
  const browser = shell("find /root/.cache/ms-playwright/chromium_headless_shell-1228 -type f \\( -name headless_shell -o -name chrome-headless-shell \\)");
  if (!browser || browser.includes("\n")) throw new Error("Expected one headless browser executable");
  const compilerDir = shell("gcc -print-libgcc-file-name").replace(/\/[^/]+$/, "");
  const executables = ["/usr/bin/gcc", "/usr/bin/as", "/usr/bin/ld", "/usr/bin/x86_64-linux-gnu-as", "/usr/bin/x86_64-linux-gnu-ld", "/usr/bin/x86_64-linux-gnu-ld.bfd"];
  const resources = [`${compilerDir}/cc1`, `${compilerDir}/collect2`, `${compilerDir}/liblto_plugin.so`];
  // NSS loads these TLS modules dynamically, so Chromium's ldd output omits them.
  const tlsModules = shell("find /usr/lib/x86_64-linux-gnu -maxdepth 2 \\( -name libsoftokn3.so -o -name libfreebl3.so -o -name libfreeblpriv3.so -o -name libnssckbi.so \\)").split("\n").filter(Boolean);
  if (!tlsModules.some((path) => path.endsWith("/libsoftokn3.so"))) throw new Error("Browser TLS modules are missing from the source image");
  const programs = [...executables, ...resources, browser, ...tlsModules];
  const dependencies = new Set();
  for (const program of programs) {
    for (const path of shell(`ldd '${program}' | awk '/=> \\/|^\\s*\\// { for(i=1;i<=NF;i++) if($i ~ /^\\//) print $i }'`).split("\n").filter(Boolean)) dependencies.add(path);
  }
  for (const program of executables) {
    const target = join(destination, program);
    await rm(target, { force: true });
    command("docker", ["cp", "-L", `${container}:${program}`, target]);
  }
  await mkdir(join(destination, compilerDir), { recursive: true });
  for (const program of resources) command("docker", ["cp", "-L", `${container}:${program}`, join(destination, program)]);
  for (const program of tlsModules) {
    const target = join(destination, "usr/lib/x86_64-linux-gnu", basename(program));
    await mkdir(dirname(target), { recursive: true });
    await rm(target, { force: true });
    command("docker", ["cp", "-L", `${container}:${program}`, target]);
  }
  command("docker", ["cp", `${container}:${compilerDir}/include`, join(destination, compilerDir, "include")]);
  for (const path of dependencies) {
    const target = join(destination, path);
    if (/\/(libc\.so|libm\.so|ld-linux)/.test(path)) {
      try { await access(target); continue; } catch (error) { if (error.code !== "ENOENT") throw error; }
    }
    await mkdir(dirname(target), { recursive: true });
    await rm(target, { force: true });
    command("docker", ["cp", "-L", `${container}:${path}`, target]);
  }
  await mkdir(join(destination, "opt"), { recursive: true });
  command("docker", ["cp", "-L", `${container}:${dirname(browser)}`, join(destination, "opt", "pilotdeck-browser")]);
  await mkdir(join(destination, "opt", "pilotdeck"), { recursive: true });
  await cp(fileURLToPath(new URL("../node_modules/.pnpm/playwright-core@1.60.0/node_modules/playwright-core", import.meta.url)), join(destination, "opt", "pilotdeck", "playwright-core"), { recursive: true });
  await writeFile(join(destination, "pilotdeck-rootfs.json"), JSON.stringify({ base, image, compilerDir, tlsModules, browser: "/opt/pilotdeck-browser/chrome-headless-shell", createdAt: new Date().toISOString() }, null, 2));
  console.log(destination);
} catch (error) {
  await rm(destination, { recursive: true, force: true });
  throw error;
} finally { command("docker", ["rm", container]); }
