# Feishu stop 正式专项候选

独立于已校准 `backend-feishu-stream`：本目录与新 `backend-feishu-stream-stop.json` 不改写正常场景、正常 raw 或成绩。当前只有源码候选，尚未执行固定 TS。产品源码保持冻结，无公共 harness 改动。

## 真正的停止完成 barrier

固定 TS `cli/pilotdeck.ts:200-206` 先 `await channelAdapters.reload(serverRef, config)`，随后才打印 `[pilotdeck] Adapter hot-reload complete: feishu=stopped`。`ChannelAdapterBundle.ts:98-113` 等待 server.reconcileChannels；实际 adapter stop 在 `FeishuChannel.ts:226-233` 中完成，但误查不存在的 SDK stop()。Rust `native_process/assembly/reload.rs:61-73` 同样在 await adapters 后打印完成信息。

新场景使用现有 `server:false` + 正式 `serve` step。其 `until` 精确等待上述完成行，`settleMs:0`；只有 ready 后，既有 run.mjs 才执行 then 里的独立正式 WS observer。不是根据 sleep 或整体进程退出猜测 adapter 停止。

## 前半与探测的因果链

- 正式启用 stream，真实 endpoint、WS、protobuf ping。mock 发送与正常前半同形的错误事件、正确逆序分片和重复消息，只有 `FEISHU-STOP-BEGIN` 进入真实模型。
- 模型回复 `FEISHU-STOP-DONE`，实际渠道发送最终 post/edit。mock 不会在只看到模型请求、临时 live 消息或任意 POST 时关闭 adapter。
- **实际最终回复后又收到客户端发出的 reaction DELETE**，证明客户端已完成前面的发送 await。DELETE response finish 后 wrapper 才把隔离 pilotdeck.yaml 的唯一 feishu enabled:true 改为 false。记录 before/after SHA，保持其他配置字节不变；实际 ConfigStore watch/reconcile 执行停机。
- 真实 reload 完成行触发 serve.then 的正式 observer turn。其实际 builtin bash 运行 seed probe，GET mock `/probe-stopped`；这条请求与工具结果都进入真实 capture。
- mock 只探测第一个实际 upgrade 所拥有的原 socket，identity 固定为 `feishu-original-socket-1`。若该 socket 已有真实 close receipt，返回 alive:false；否则向它发送随机 24-byte nonce 的 **WS control Ping**，只有收到同 socket、相同 nonce 的 Pong 才返回 alive:true。等待中的真实 close 也可以返回 false。写失败本身不是 dead 证明；3s 无 close/Pong 返回 504，不能当成功。
- 原 socket 后来因产品整体退出而 close 不影响已记录 probe；strict 明确要求 TS 的 Pong 在 post-reload probe 中发生，整体退出后的 close 在 probe 之后。

## 窄差异的正向期望

按 `H1-FEISHU-STREAM-OWNERSHIP-001`，strict 对两侧分别要求：

- TS：alive:true，evidence:`exact-nonce-pong`，raw 必须有实际 Ping/Pong nonce 对应。
- Rust：alive:false，evidence:`socket-close`，raw 必须有隔离配置关闭后、probe 成功前的原 socket close。

同一 receipt 必须实际进入 observer-final 模型工具结果；实际正式历史和 durable input 也检查。不存在整段忽略、volatile 或本目录自行添加的公共 accepted 规则。公共 pair 预计会报告该明确差异，其精确允许值策略需根协调者/公共 comparator owner 接线，不能拿 strict 通过冒充 raw pair SAME。serve stdout/stderr 仍完整保留并参与公共比较，额外差异应独立分析，不因本条自动豁免。

## 隔离与命令

沿用已校准的 runtime CA / Axios absolute URL / Rust CONNECT 本地 mock，源码复用正常目录的 closeBarrier（其 SHA 进入 receipt），CA 在运行时生成、不入库。仅 E0 Docker `--network none`；wrapper 强制 Linux 且无非 loopback interface，拒绝任何外网 forward。不开宿主端口、不碰真实账号。四件套都要回传。

```sh
node scripts/bbx/fixtures/feishu-stream-stop/capture.mjs ts /evidence/feishu-stop-ts-first.json
node scripts/bbx/fixtures/feishu-stream-stop/verify.mjs /evidence/feishu-stop-ts-first.json --self-test
node scripts/bbx/fixtures/feishu-stream-stop/capture.mjs ts /evidence/feishu-stop-ts-second.json
node scripts/bbx/fixtures/feishu-stream-stop/verify.mjs /evidence/feishu-stop-ts-second.json --self-test
node scripts/bbx/diff.mjs /evidence/feishu-stop-ts-first.json /evidence/feishu-stop-ts-second.json --self
```

先固定 TS 双轮校准，再 Rust；不重算原正常场景成绩。strict 包含五项结构负控：缺渠道请求、缺实际 reload 完成行、反转 alive、换 socket identity、缺 raw probe-result。

实际负控：`capture.mjs ts OUTPUT --wrong-nonce` 只让探测器期待另一个 nonce，真实客户端即便返回 Pong 也不能被认作对应证据，必须 504/strict 拒绝，不得超时假报 dead。该变体未运行，不能标通过。

## 尚未验证

Node 静态语法通过；正式 TS capture、TS self、actual wrong-nonce、Rust 停止 proof 均待 E0。若当前源版本日志或 ConfigStore 行为与源码推断不符，先保存并定位首败，不改公共标准让其通过。
