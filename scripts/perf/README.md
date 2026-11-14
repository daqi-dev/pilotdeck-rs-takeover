# perf：性能报告用的测量脚本

交付标准第 7 条（`specs/delivery/README.md`）要的数字由这里的脚本量。两侧起服务的方式与 `scripts/bbx/run.mjs` 相同：基线是 `node <BBX_BASELINE>/dist/src/cli/pilotdeck.js server --port P`，Rust 是 `<PERF_TARGET>/pilotdeck server --port P`（`PERF_TARGET` 缺省为 `src/target/release`）。每次都在 `os.tmpdir()` 下新建的临时目录里跑，HOME、PILOT_HOME、TMPDIR、项目目录都在里面，配置与 bbx 的缺省配置相同；不读写 `~/.pilotdeck`，拒绝 18789 端口。

## coldstart.mjs：冷启动

```
BBX_BASELINE=/path/to/PilotDeck node scripts/perf/coldstart.mjs ts   --runs 30 --out ts.json
                                 node scripts/perf/coldstart.mjs rust --runs 30 --out rust.json
```

量的是从 `spawn` 到 stdout 出现 `PilotDeck server listening:` 的墙钟毫秒数，量到即 SIGTERM。缺省每轮一个全新的 home（首次启动）；`--reuse-home` 让各轮共用一个 home（先热身一次不计入）。输出中位数、四分位、p90、最小最大和前后的 loadavg，`--out` 另存每轮原始值。

注意：

- macOS 上一个刚链接或刚复制出来的可执行文件，第一次 exec 要多付约 0.7–1.1 s（系统对新二进制的首次检查，与文件大小有关；53 MB 的 `pilotdeck` 约 1.1 s，strip 后 42 MB 约 0.7 s），之后不再付。脚本不剔除它：刚构建完就量，第一轮就是这个数。比较代码改动时先跑一次热身，或看中位数。
- 机器上有别的负载时数字会抖，报告里写上 loadavg。

## idle-cpu.mjs：空闲 CPU

```
node scripts/perf/idle-cpu.mjs rust --window 60 --rounds 2 --out idle-rust.json
```

起服务、等到监听行、再静置 `--settle` 秒（缺省 5），然后量 `--window` 秒（缺省 60）内整个进程组（含子进程）`ps` 累计 CPU 时间的差。`cpuPct` 是单核百分比。
