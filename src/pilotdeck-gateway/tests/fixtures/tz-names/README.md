# 时区名夹具（get_current_time / cron）

`oracle.json` 由 `record.mjs` 在 Node v22.22.0（ICU 78.2，tzdb 2025c）下直接调用固定基线
（`cc7c4e6`，已构建的 `dist/`）录制，供 `tests/tz_names.rs` 回放：

- `cases`：`names.json` 里每个名字（规范名、`backward` 旧别名、ICU 独有的 `PST` / `IST` /
  `SystemV/*` 等、大小写变体、带空格、UTC 偏移、ICU 拒绝的名字）在两个时刻的
  `get_current_time` 结果或错误、`isValidCronTimezone`、`computeNextCronRunAt("0 9 * * *")`；
- `accepted` / `rejected`：`candidates.json`（内置 tzdb 的全部名字、ICU 独有名字、ICU 拒绝的名字）
  逐个交给 `new Intl.DateTimeFormat("en-US", { timeZone })` 的结果，接受的附 ICU 的
  `resolvedOptions().timeZone`；
- `offsets`：每个 ICU 区在 1900–2099 年 72 个采样时刻的 UTC 偏移（秒）。

基线对显式给出的时区名只用 Node 自带的 ICU 数据，不读系统 `/usr/share/zoneinfo`；Rust 侧从
`pilotdeck-transport` 内置的 tzdb（`jiff-tzdb` 0.1.4，tzdb 2025b）解析，所以这组测试与本机是否装了
`tzdata-legacy` 无关。

再生成（仓库根目录，Node v22.22.0）：

```sh
node src/pilotdeck-gateway/tests/fixtures/tz-names/record.mjs /abs/PilotDeck > src/pilotdeck-gateway/tests/fixtures/tz-names/oracle.json
```

连续两次输出字节一致才更新。用 ICU 77.1（tzdb 2025b）的 Node v22.22.0 录出的结果只有
`versions` 和 America/Tijuana 1960 年的两个偏移不同（tzdb 2025c 修订了该区 1953–1975 年的历史），
测试里对这一段做了说明并跳过。
