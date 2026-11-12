WhatsApp 原生候选与固定基线夹具

参考是 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，Node 必须为 v22.22.0。基线只读；驱动不安装包、不编译基线、不启动真实 WhatsApp 服务、不读真实 home。所有 server 和 mock 均使用随机 localhost 端口、独立 HOME/PILOT_HOME 和白名单环境。

`capture.mjs` 是正式 `pilotdeck server` 驱动；`value-oracle.mjs` 与 `interaction-oracle.mjs` 是模块层特征测试，不能代替正式入口。这里没有 Matrix/WhatsApp 平台 SDK，也不假造 WhatsApp 登录协议：固定 TS 渠道本身只是用户提供的外部 Node bridge 的 HTTP 客户端和进程管理者。

| 文件 | 检查对象 |
| --- | --- |
| `capture.mjs`、`bridge.mjs` | 正式入口的缺配置、成功回复/会话/去重/失败发送、ready 404、热更新、cron、活动 turn 禁用 |
| `compare.mjs` | 五场景分母、重复/缺失、退出/信号、强杀、bridge 所有权、发送正文/状态、模型用户消息和活动 turn 取消 |
| `value-oracle.mjs` / `.json` | 17 个 JSON envelope/别名/空值/UTF-16 案例，6 个 mapper 步骤，15 个渲染值，8 个构造输入 |
| `interaction-oracle.mjs` / `.json` | 真实 TS adapter + helper + 本地 HTTP；elicitation、连续 permission、首次提示发送失败重试。Gateway 事件是脚本输入，adapter 设置到明确的启动后模块测试点 |
| `process-bridge.mjs` | Rust 进程所有权测试的外部工具：大 stdout、忽略 TERM、带孙进程；不是产品运行时文件 |

在仓库根目录，用新输出目录运行固定 TS：

```sh
NODE22=/absolute/path/to/node
PDRS_BASELINE=/absolute/read-only/PilotDeck "$NODE22" src/pilotdeck-gateway/tests/fixtures/h1-whatsapp/capture.mjs _tmp/whatsapp-ts-a
```

`NODE22` 在这里只是 shell 变量，驱动严格检查真实 `process.version`。传入的 baseline 要同时具有固定源码、依赖和现存 dist。驱动检查 Git HEAD、src/packages 已跟踪文件无修改，并用固定源码的已装 TypeScript 将相关源码在内存转译，与 dist 逐字节比较。每轮保存入口/Node/driver/bridge/源码/dist SHA、命令、环境、请求、stdout/stderr、退出与清理结果。

公共装配完成后，Rust 使用同一个驱动和同一个外部 bridge：

```sh
PDRS_BASELINE=/absolute/read-only/PilotDeck PDRS_RUST_BINARY=/absolute/native/pilotdeck "$NODE22" src/pilotdeck-gateway/tests/fixtures/h1-whatsapp/capture.mjs _tmp/whatsapp-rust-a
"$NODE22" src/pilotdeck-gateway/tests/fixtures/h1-whatsapp/compare.mjs _tmp/whatsapp-ts-a/results.json _tmp/whatsapp-rust-a/results.json
```

完整比较要求恰好五个场景。捕获可用第三参数选择逗号分隔的已知唯一子集，供定位使用；子集报告不能通过完整比较。基线中的活动 turn 禁用后继续执行，原生期望取消并排空该准确 turn；其余发送和模型请求按实际捕获逐项比较，不能把持有的请求漏掉或用新 session 的取消冒充。

负控分两层：

```sh
"$NODE22" src/pilotdeck-gateway/tests/fixtures/h1-whatsapp/compare.mjs --self-test _tmp/whatsapp-ts-a/results.json
PDRS_NEGATIVE_REPLY=1 "$NODE22" src/pilotdeck-gateway/tests/fixtures/h1-whatsapp/capture.mjs _tmp/whatsapp-wrong-reply whatsapp-basic
```

第一条对捕获数据做七种破坏：正文、漏项、重复项、信号退出、强杀、活动 turn 未取消、cron chat 错误；比较器必须全部拒绝。它使用合成候选验证比较器，不是 Rust 产品通过。第二条在真实正式 TS 链的本地模型 mock 中注入错误回复；捕获必须退出 1、保留断言失败和原始结果。未修改基线产品源码，也不把这两项描述成 Rust 产品源码 mutation。

模块 oracle 重跑：

```sh
"$NODE22" src/pilotdeck-gateway/tests/fixtures/h1-whatsapp/value-oracle.mjs _tmp/whatsapp-values-a
"$NODE22" src/pilotdeck-gateway/tests/fixtures/h1-whatsapp/interaction-oracle.mjs _tmp/whatsapp-interactions-a
```

现存 `.json` oracle 已分别连续两轮逐字节相同。Rust 组件测试读取这些原始期望，覆盖归一化、mapper、渲染、完整交互提示/答案与失败重试；另外测试并发 busy/dedup、单消息长 UTF-16 正文、provider 失败、headers-only readiness、重叠 poll、60 秒发送超时、cron 外部调用者丢弃、可重试 owned cancel 和实际 bridge 资源释放。

Rust 候选的外部接口仍是 `extra.bridgePath` / `extra.bridgeUrl` 和原来的环境变量；没有新增配置字段。默认 native 核心无 Node 依赖，只有启用 WhatsApp 且提供 bridgePath 时才从 PATH 解析 Node，执行用户脚本。禁止把 Rust 当前可执行文件当作 Node，禁止调用旧 TS adapter/Gateway/Agent。公开 Rust options 的 executable/env/transport 仅供 embedding 和隔离测试。

本包请求统一登记的三个 bug 修复：

- 固定 TS 把 stdout pipe 留着不读。真实模块+实际 Node bridge 两轮在先写 4 MiB stdout、再 listen 的脚本上分别约 15.3 秒 readiness 失败。候选并行有界排空 stdout/stderr。
- 固定 TS 只 kill 直接 child。真实模块两轮 stop 返回后，忽略 TERM 的孙进程仍存活；探针仅对自己记录的 PID 做清理。候选 Unix process group 用 waitid NOWAIT 保留 leader 所有权，最后一次组信号后才 reap，避免向复用 PGID 发信号；TERM 后最多 5 秒升级 KILL。
- 固定 TS 停渠道仅 abort poll/kill bridge，不取消被接受的模型 turn。正式 server 两轮禁用后仍有活跃 SSE；候选使用共享 `cancel_and_drain`，并独立拥有清理任务，不能由 stop/cron/start 的外部等待者丢弃来遗失所有权。

原始证据在 `_tmp/20261113-h1-next-matrix-whatsapp/`。`formal-a`/`formal-b` 保留最初 abrupt Gateway WebSocket 关闭造成的主进程 8 秒强杀失败，精确旧探针为 `probe-abrupt.mjs`；正常 WebSocket close 握手后正式场景自然退出。该问题不能归因成已修好的 WhatsApp bridge 问题，也未据此修改 Gateway 公共代码。

交付限制：此候选尚未公共挂载，未从拒绝表移除；当前作者按协调要求没有运行 Cargo。语法格式检查与 TS 证据不代表 Rust 编译、测试或正式 native 对拍通过。主协调者应在独立批次挂载、集中 check/受影响测试、正式对拍并补齐差异账本。当前进程组释放设计和实际进程测试面向本轮 Linux/macOS 目标；Windows 分支未验收，不能宣称具备 Unix 的进程组保证。


恢复批次补充：`start-oracle.mjs`的前三格真实NUL参数同步spawn失败与第4格真实Node异步ENOENT（仅executable/readiness seam）分别验证不同日志/返回路径。正式server不由此冒充通过。`start-oracle.json`由2026-11-13 c/d两轮固定Node22.22.0捕获，hash相同。Rust新增两项因果测试，现22项等待集中编译。普通测试HTTP mock URL由Fetch禁止端口1改为18081，确保请求确实进入mock；精确URI断言同步修改。
