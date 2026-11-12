# O1 Telemetry oracle

固定 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`、Node 22.22.0。只用新建隔离 HOME/PILOT_HOME、假 token、127.0.0.1 随机端口；拒绝 18789。全部模块请求由 fetch 守卫拒绝非回环地址，正式 server 明确配置模型和 analytics 地址为本地 mock，不会依赖默认外发地址。

`capture.mjs` 是真实 Collector/Sender/Registry/Gateway Observer 模块加真实 HTTP 的模块层证据。九格覆盖：事件字段和过滤、metadata 覆盖、Error 双事件、失败批次逐项 unshift 的逆序及指标、JSONL 非法行/非整数 attempts/超容量/禁用恢复、两个 enabled timer 别名分支、shutdown 在途丢失风险、真实请求超时释放、28 个 Gateway 派生事件、Registry 替换/错误隔离/排空及 wrapper 单次观察。

`formal.mjs` 启动实际 `pilotdeck server --port 0 --host 127.0.0.1`，通过公开 WebSocket hello/submit_turn 进入 Gateway；不导入或替换产品类。配置 enabled=true 覆盖 env false；正常 turn 的四条遥测顺序为 loop_start → model_request → model_response → loop_end。定时器和 batch size 使事件保留到 SIGINT，正式 shutdown snapshot 是 queued=4/sent=0/queueDepth=4，之后实际 POST 四条、退出 code=0，queue.jsonl 不存在。

运行示例（从 Rust worktree 根目录；每次 `--out` 必须不存在）：

```sh
/opt/homebrew/Cellar/node@22/22.22.0_1/bin/node src/pilotdeck-gateway/tests/fixtures/o1-telemetry/capture.mjs --baseline /Users/da/ws/pilotdeck-rs/resources/PilotDeck --out _tmp/20261113-o1-telemetry/module-next
/opt/homebrew/Cellar/node@22/22.22.0_1/bin/node src/pilotdeck-gateway/tests/fixtures/o1-telemetry/capture.mjs --baseline /Users/da/ws/pilotdeck-rs/resources/PilotDeck --out _tmp/20261113-o1-telemetry/module-repeat --compare _tmp/20261113-o1-telemetry/module-next
/opt/homebrew/Cellar/node@22/22.22.0_1/bin/node src/pilotdeck-gateway/tests/fixtures/o1-telemetry/formal.mjs --baseline /Users/da/ws/pilotdeck-rs/resources/PilotDeck --out _tmp/20261113-o1-telemetry/formal-next
```

模块负控加 `--mutate unexpected-success`：mock 将重试场景真实 HTTP 503 改成 200，产品后续请求从 AB/BA/AB/C 改为 AB/C，原断言必须失败。正式负控加 `--mutate model-output`：修改真实模型返回，公开 WS 输出断言必须失败。

Rust 正式对拍用同一 formal 命令增加 `--rust-bin <绝对二进制> --rust-root <绝对源码目录> --rust-sha <40位提交> --compare <同平台TS正式结果目录>`。记录完整命令、候选提交、可执行文件 SHA、入口 SHA、隔离环境；源码尚有未提交修改时，应由主协调者另外保存候选源码清单与 SHA。作者未运行 Rust 命令。

本机原始材料在 `_tmp/20261113-o1-telemetry/`：

- module-first：九格首跑通过，后续补上动态端口和输出目录的精确归一化。
- module-second / module-fourth：九格精确自比通过；`module-oracle.json` 来自 fourth。
- module-third：比较内存 undefined 与 JSON 落盘缺字段导致驱动失败；原始材料保留，比较层已统一为 JSON 表示。
- module-negative：实际 HTTP 状态负控捕获，exit 1。
- formal-first / formal-second：正式入口两轮 contract 精确相同，退出 code 0；`formal-oracle.json` 来自 second。
- formal-negative：真实模型输出负控捕获，exit 1；failure-cleanup 信号单独记录，不能算正常退出通过。

所有 raw 请求正文、HTTP headers、socket 事件、stdout/stderr、WS frame、PID/信号、JSONL 和 metrics 均保留。只归一化已验证格式/公式的 UUID、ISO 时间、instanceId、同一 runId、指定回环端口和隔离输出路径；installation/session hash、业务字段和事件顺序精确比较。source-identity 核对固定 HEAD、对应 tracked 源码 clean、12 个实际执行模块与已安装 TypeScript 转译结果，只忽略 sourceMap 尾注和外围空白；它不证明完整 dist/dependency 树重新构建。

Rust 修复 shutdown 在途事件丢失属于主协调者已授权窄差异：同步停 timer、等待同一个 owned flush receipt 确定结果后 persist，外层 shutdown future 被 drop 不丢失 owner。普通 flush 在 flushing 时仍立即返回，enabled 别名的定时器行为保持基线。该修复的 Rust 因果测试尚未运行，正式 in-flight Rust/TS 差异验收也待主协调者安排。
