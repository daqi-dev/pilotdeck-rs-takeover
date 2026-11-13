# Feishu stream 正式入口候选

本目录是正式 CLI/gateway 外围的独立本地 mock 装置，不 import 产品模块、不替换 SDK、不增加产品配置 seam。源码候选尚未执行 TS 首轮，不宣称通过。与已跑的六项 Rust 私有 loopback 测试独立计数。

## 路径与已核实能力

固定 TS `FeishuChannel.ts:257-269` 使用固定 Feishu origin、实际 Lark SDK WSClient；SDK `lib/index.js:89072-89109` 经 Axios 请求 `/callback/ws/endpoint`，不是原生 fetch。TS `cli/proxy.ts` 的 undici dispatcher 不能代替 Axios 的代理。mock 同时处理 Axios 环境代理的 absolute-URL HTTP request 和 Rust / undici 的 CONNECT；只接受 `open.feishu.cn`，不透传任何公网 origin。endpoint 返回同一隔离网络内的 loopback WS URL，SDK 无需额外 WS proxy agent。

Rust `runtime/model/http/tls.rs:47-111` 明确读取 `NODE_EXTRA_CA_CERTS`，共享 HTTP/WS connector 使用该 roots 快照；Node 启动也读取同一变量。wrapper 使用系统 openssl 在临时目录运行时生成测试 CA 和服务证书，含 SAN、serverAuth；不设置 TLS 校验禁用，不提交私钥、证书，不下载工具。

## 强制隔离与执行

仅 E0 排队，在 Linux Docker `--network none` 的单独命名空间运行。wrapper 检查 Linux 且全部地址均为 internal loopback；有非 loopback interface 即拒绝，没有绕过选项。真实 18789 listener 不可能来自宿主命名空间。TLS/proxy/WS 全部监听 127.0.0.1，slot 对应端口为 `38413/38414/38415 + 10*BBX_SLOT`，不映射宿主端口。

```sh
node scripts/bbx/fixtures/feishu-stream/capture.mjs ts /evidence/feishu-ts-first.json
node scripts/bbx/fixtures/feishu-stream/verify.mjs /evidence/feishu-ts-first.json --self-test
node scripts/bbx/fixtures/feishu-stream/capture.mjs ts /evidence/feishu-ts-second.json
node scripts/bbx/fixtures/feishu-stream/verify.mjs /evidence/feishu-ts-second.json --self-test
node scripts/bbx/diff.mjs /evidence/feishu-ts-first.json /evidence/feishu-ts-second.json --self
```

同时设置 E0 已固定的 `BBX_BASELINE`、`BBX_TARGET`、`BBX_SRC`、`BBX_TMP_ROOT`、`BBX_SLOT`。wrapper 仍调用原样 `run.mjs`，由其隔离产品 HOME/PILOT_HOME、守卫端口、执行真实 CLI 与保存实际 artifact。每轮输出 capture、`.run.log`、`.mock-raw.jsonl`、`.receipt.json` 四件套；错误退出不得删除首败。CA 在该轮结束后精确删除。

不要跳过 wrapper 单独调用 run.mjs：未启动 mock/生成 CA 时只会得到环境失败。wrapper 拒绝复用已有私有 lock；异常遗留 lock 由 E0 核对其中精确 PID 后处理，不自动杀任意进程。

## 因果链与严格验证

1. 正式 `adapters.feishu.enabled=true/connectionMode=stream` 启动真实 SDK/native endpoint 与 WS；mock 收到第一个实际 protobuf ping 才注入消息。
2. 依次发送错误 event_type、正确事件的逆序双片、相同 message_id 的完整重复。只有正确正文 `FEISHU-STREAM-BEGIN` 可以成为模型请求与 durable accepted_input。
3. 真实 provider 回 `FEISHU-STREAM-DONE`；真实渠道调用 token/reaction/send Open API，mock 只接受已注册路径与本地 token。
4. 独立正式 WS observer 通过实际 builtin bash 运行 probe，等待 mock **已收到包含最终正文的 create/edit 请求** 后才返回 receipt。控制请求不是 sleep，也不以“connected”日志或最终 tree 的存在制造成功。随后正式 read_session_messages 必须读到相同输入和回复。
5. strict 校验端点字段、真实 WS upgrade、固定 service ping、三项 ACK、未知事件返回值、模型请求单次性、正式历史、落盘和实际出站内容。

所有实际 HTTP 头/正文、原始 WS 握手和二进制帧均保存在原样 raw sidecar。collector 内的 mock facts 是独立观测投影，strict 从原始帧/HTTP 字节重建后逐项校验。只有 ACK 的 `biz_rt` 被转为数值 `durationMs`，沿用现有比较器的耗时归一化；原始值仍留在 raw sidecar，不修改产品 capture，不注册整 URL / header / ACK volatile。正常 facts 没有记录 adapter stop 成功；WS close 仅原样写入 raw sidecar，不作正常阶段通过标准。

wrapper receipt 固定场景字节、四个 fixture 源文件 SHA、raw SHA、artifact SHA 与真实采集器退出状态；strict 要求对应当前候选字节，不能用旧 capture 或改写后的 raw 充当同输入证据。该覆盖只声明上述合同字段，不声称验证所有未被列出的 HTTP 头或所有 Feishu 功能。

## 真实负控

```sh
node scripts/bbx/fixtures/feishu-stream/capture.mjs ts /evidence/feishu-wrongreply.json --wrongreply
node scripts/bbx/fixtures/feishu-stream/verify.mjs /evidence/feishu-wrongreply.json
```

该变体只把 provider `channel` 的真实回复改成 `FEISHU-STREAM-ACTUAL-NEGATIVE`，实际渠道仍应发送并被 probe 观察，capture 应能正常结束；strict 固定期待正常出站正文，必须拒绝。`--self-test` 另有四项结构 corruption，不能替代真实 wrongreply capture。

## 尚未闭合与停止专项

当前仅源码 ready，Node 语法检查通过；TLS / Axios 双代理、首轮正式 TS capture、TS self 和真实负控均待 E0。首败必须先分清装置与产品，不盲重跑。

停止差异单独按 `H1-FEISHU-STREAM-OWNERSHIP-001` 验收。需要真实禁用 reload **完成 receipt** 后，再由独立实际工具查询旧 socket 精确 identity 的存活状态；TS 原实现泄漏与 Rust 关闭的差异要逐值登记。当前 wrapper 不把 product 整体退出后的 close 当作 adapter stop 证明，也未加入没有完成 barrier 的盲 sleep stop 场景。先校准正常阶段，再核 TS/Rust reload 的真实 completion 观测点并实现该专项，不以本文替代测试。
