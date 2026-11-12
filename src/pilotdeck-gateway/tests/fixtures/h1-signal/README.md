# H1 Signal 证据与回归

固定基线：OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85，Node v22.22.0。本目录仅使用本地 HTTP bridge / 模型 mock，账号和 API key 都是夹具常量。

## 正式 server

capture.mjs 只启动真实 dist/src/cli/pilotdeck.js server --port 0 --host 127.0.0.1，不导入、替换或注入产品模块。所有端口由内核分配，不使用 18789。HOME、PILOT_HOME、配置、cwd、TMPDIR、技能/插件路径均位于每次全新的输出目录；仅对自己 spawn 得到的 PID 发信号，保存实际退出。观察截止不算成功。

从工作树根运行（输出目录必须不存在）：

    node src/pilotdeck-gateway/tests/fixtures/h1-signal/capture.mjs --baseline /path/to/fixed/PilotDeck --out _tmp/h1-signal-traffic-a
    node src/pilotdeck-gateway/tests/fixtures/h1-signal/capture.mjs --baseline /path/to/fixed/PilotDeck --out _tmp/h1-signal-traffic-b --compare _tmp/h1-signal-traffic-a
    node src/pilotdeck-gateway/tests/fixtures/h1-signal/capture.mjs --baseline /path/to/fixed/PilotDeck --out _tmp/h1-signal-negative --mutate model-output

负控必须非零退出，且首个真实 Signal POST 的 message 为 injected-wrong-answer 并触发断言。它改变模型 mock 输出后经过产品路径，不是仅修改已采集 JSON。失败后的 TERM 清理不能当作产品取消通过。

正文覆盖分片 UTF-8、CRLF、空行/非法 JSON、SSE sentinel、/new、同步消息优先级、群聊回复到 source、2000 UTF-16 code unit 分块（包括拆开 emoji）、忙时跳过、正常 EOF 重连和 SIGINT。落盘 runs/registry.json 必须含五个不同 run ID；DM general、group general、新建 UUID session 的关系和后续复用均断言，模型 user history 逐轮比较。只归一化已经匹配 UUIDv4 的新建 session ID。

    node src/pilotdeck-gateway/tests/fixtures/h1-signal/capture.mjs --baseline /path/to/fixed/PilotDeck --out _tmp/h1-signal-reload-a --scenario reload-held
    node src/pilotdeck-gateway/tests/fixtures/h1-signal/capture.mjs --baseline /path/to/fixed/PilotDeck --out _tmp/h1-signal-reload-b --scenario reload-held --compare _tmp/h1-signal-reload-a

reload 场景挂住真实 provider 请求，写隔离配置禁用 Signal，等待正式 watcher 输出 signal=stopped。关闭前后采集接收流、provider 响应连接、同一个 server PID 和 registry，再请求 /health 证明 server 未退出。基线实际观察：接收流关闭，500ms 后旧 provider 连接仍开且 turn 仍运行；重新启用 adapter 后旧连接仍存在。之后显式完成 mock 响应，旧 adapter 不发送晚到结果，新 adapter 可接收下一条输入，最后 SIGINT 退出。该格不依赖整进程退出观察资源释放。

identity.json 保存命令、隔离环境、平台、Node 和执行文件 hash。source-identity.json 对入口、bundle、loader、Signal 三文件和三个交互/cron helper 共九个源文件检查固定 HEAD、tracked clean，再用已安装 TypeScript 按 ES2022/ESNext 只读转译，逐字核对实际 dist（只忽略 sourceMappingURL 尾注和外围空白），记录源码/输出 hash 与编译器版本。它证明这九个执行模块对应基线，不冒充完整 ignored dist/dependency 树的干净重建。

原始材料含 network.jsonl（完整主模型及 session-title 辅助请求）、stdout/stderr 二进制、初始/最终每个文件的 base64/hash、退出与信号、session-contract.json，以及 reload 的 ownership.json。全部材料保留，但当前只断言正文/会话业务子集，不声称全量持久化对拍完成。

## 补充模块层

    node src/pilotdeck-gateway/tests/fixtures/h1-signal/features.mjs --baseline /path/to/fixed/PilotDeck --out _tmp/h1-signal-features-a
    node src/pilotdeck-gateway/tests/fixtures/h1-signal/features.mjs --baseline /path/to/fixed/PilotDeck --out _tmp/h1-signal-features-b --compare _tmp/h1-signal-features-a

features.mjs 加载真实 TS Signal 类和真实 HTTP bridge，但 Gateway 是脚本化事件源，不能算正式 server。六组覆盖：

- 初始权限提示 POST 失败→重送→决策 RPC 失败→再次回答；elicitation 的孤立 UTF-16 问题键/选项和已回答时无额外确认。
- cron 两段发送，首段失败后仍发送第二段，返回 false；外渠道 origin 不投递。
- cron 首段真实 POST 挂住时，基线 stop 仍返回，POST 保持打开；释放首段响应后还发送第二段并返回 true。该格明确是模块层资源/晚发证据。
- receive HTTP 503 后至少 2.9s 才重连；正常 EOF 后 JSON 行 carry 跨连接继续。
- 基线 stop 返回后 active chat 仍存在，直到脚本显式释放 turn；stop 后不发送晚回复。
- 同一真实 undici reader 结果内两条 JSON 行，立即结束的空 async iterator 仍使第二条 busy。该格在原生 read Promise 上添加只读观察，记录原始读取块，不修改 bytes/status/chunk 边界；仍然只算模块层时序证据。

## 已有证据及限制

formal-oracle.json 来自本机 formal-second/contract.json；features-oracle.json 来自 features-seventh/oracle.json。原始材料位于工作树 _tmp/20261113-h1-signal/：

- formal-second / formal-third：正式正文两轮通过；formal-negative：实际 mock 变异被捕获。
- session-retrospective.json：对上述两轮原始 registry/model 请求追补会话和历史断言，两轮一致。
- reload-first / reload-second：正式热更新两轮通过。
- features-fifth / features-sixth：五格模块层两轮完全一致；features-seventh / features-eighth 加入真实挂起 cron POST / stop 后晚发场景，六格两轮完全一致。
- 首次失败保留：formal-first 是驱动误将非 stream 标题请求挂起；features-first.log 误断言已回答 elicitation 多发确认；features-second 是 mock 重连时的 heartbeat 与保留 carry 合并，随后驱动改为仅 flush headers。上述均为驱动问题。

Rust 专项在 src/pilotdeck-gateway/src/channel/signal/tests.rs。作者仅做文件级 rustfmt，未运行 cargo。候选生产 mount、正式 Rust server 对拍、cancel/drain 窄差异登记及最终构建由主协调者统一完成；这些 TS 与模块证据不等于 Rust 已交付。

停止接口的审查修订：stop 调用同步停止入站，由持久 cleanup worker 持有 reader、handler 和待重试 stream；返回 future 只等待共享结果。新增 Pending 等待被丢弃后继续等待真实 drain、worker 意外终止保留明确错误、完成 handler 的 panic 经下一批收割仍不能被吞掉等因果测试。当前共 16 项 Rust 专项，仍待主协调者运行。所有出站 HTTP 另由 adapter-owned worker 持有，外层 cron caller 只持结果 receipt；发送登记与 stop 关门共用同步锁，stop join 实际发送任务。新增真实随机 TCP pending POST 测试覆盖 caller 保留但不再 poll、caller 被 drop，以及 peer EOF/reset；worker panic 必须 reply false 且 stop 保留错误。生产注册另提供 register_with_network，将主机当前 ChannelNetwork 注入 options.transport，渠道不复制代理配置状态。

候选构建后，正式驱动可以添加以下参数运行 Rust：

    --rust-bin /candidate/src/target/debug/pilotdeck --rust-root /candidate --rust-sha FULL_40_CHARACTER_SHA

入口仍为 server --port 0，配置、mock、输入相同；可用 --compare 指向 TS 对应场景的目录。驱动记录候选 Git SHA 与二进制 hash，二者的构建对应关系仍由 E0 构建身份材料证明。reload-held 的 Rust 侧明确要求 disable 完成后 owned provider 连接已关闭，TS 侧明确记录其仍开；其余正文合同不为取消差异放宽。该不同期望需主协调者的窄差异登记才能计为最终通过。
