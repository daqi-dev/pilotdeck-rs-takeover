# SDK 真实信号取消的具名差异策略

本策略仅实现 `D1-SDK-SIGNAL-ABORT-ORDER-001`，仅用于 `c1-sdk-cancel-resume`。固定 TS 自比的原始 DIFF 和首败继续保留，不能利用本策略将其变为 SAME。

## 正式入口用法

先对即将用于正式采集的同一封存 Rust CLI 执行完整八路 wire 验证。用法见 `src/pilotdeck-cli/tests/fixtures/sdk-signal-abort-order/README.md`。Linux 必须有 Linux binary 自己的验证和收据，不能借用 macOS 收据。

在正式 supervisor 启动前设置两个绝对证据路径：

```sh
BBX_SRC=/absolute/path/to/fixed-candidate-checkout \
BBX_TARGET=/absolute/path/to/sealed-target-directory \
BBX_BASELINE=/absolute/path/to/fixed/PilotDeck \
BBX_SDK_SIGNAL_WIRE_DIR=/absolute/path/to/original-complete-eight-wire-output \
BBX_SDK_SIGNAL_WIRE_RECEIPT=/absolute/path/to/original-verifier.stdout \
node scripts/bbx/check.mjs --out /absolute/path/to/NEW-formal-output \
  scripts/bbx/scenarios/c1-sdk-cancel-resume.json
```

`BBX_TARGET/pilotdeck` 的实际 SHA-256 必须与八路证据相同；formal 输出目录必须全新。上述命令不安装依赖、不授权构建或真实账号访问，不启动宿主固定端口服务。E0 继续负责构建、轻槽和真实 Gateway 的私网 Docker 调度。

应优先使用 `check.mjs`，它同时验证正断言、来源/源码/binary 身份、worker 状态和 manifest。裸 `diff.mjs` 对这个具名场景也会校验固定声明；双方同时删除/改变声明、缺失收据或原始证据时为 INVALID，而不是可忽略的普通 DIFF。其他行为差异继续普通比较。

## 重验真实原件，不信手写 PASS

`evidence.mjs::loadSdkSignalWireEvidence` 只使用操作者通过环境参数明确给出的绝对目录/收据路径，不使用 artifact 提供的可执行程序或任意文件名列表。原始文件名单固定；拒绝 symlink 和非普通文件，raw 元数据中的路径穿越不能选择额外文件。

使用收据前依次执行：

1. 核当前受信 driver/verifier 的实际字节哈希，要求已实跑版本：driver `6b29ef61cf58a1403dbfc15e55f057c65ee8b1ef0afa3272f48a7a41d2d8926a`，verifier `e4e2d422e7b96d2b67219fec81510edb329e0566b308752d95add440ef1edc87`。
2. 核原 wire identity 的 producer hash，以及 CLI SHA 与实际正式 binary 相同。
3. 仅调用这个受信 verifier，对原八路 raw 全量重新执行严格验证；保留自然退出、真实 signal/current-run RPC、准确成功/错误结果和两 socket 释放判据。
4. 重验得到的 PASS/binary/精确八场景对象必须逐值等于原始收据。单独手写 PASS、没有完整有效 raw 不能通过。
5. 验证后再次散列原始收据及全部 34 个 raw 文件，期间变动则拒绝。

`inputIdentity()` 在 `sdkSignalWire` 中保存 driver/verifier 哈希、binary SHA、原收据路径/哈希和各 raw 哈希。artifact 验证及公共比较再次重验，要求所记录身份未改变。缺路径/文件、过期 binary/source、篡改收据或 raw 都是 INVALID，没有软跳过或只按路径放行。

身份保留原绝对路径，验证主机/容器必须继续能访问原件。跨主机审计须明确保全证据位置及来源，不能静默换另一平台的收据。

## 两层正判与映射边界

场景声明具名策略，并执行两个 Rust-only 正断言：准确 aborted footer、空 stderr。原 SIGINT、timeout、settle、持久事件触发、全部业务 turn 和恢复/历史检查保持不变。

策略独立重放原始持久 registry 触发，检查 signal/PID/时间、同 session/project/request/run 的 partial、当时 provider open 且已 flush一块、最终唯一三条 durable run、同会话恢复、partial 恰好持久化一次，以及 provider 调用/恢复上下文没有重复或缺失。

TS 只接受已经取证的 clean-aborted footer，或 C1 partial footer加固定 SDK 未处理关闭堆栈的准确字节；任意 Gateway-client-closed 文本、其他栈、附加错误及未经取证的路径/运行时变体都不放行。两侧已执行 assertion 记录必须与当前场景重算结果一致。

所有检查通过后，只可能返回：

- `$.steps[1].serve.stdout`：仅 footer；此前归一化 stream 事件的全部字节必须相同。
- `$.steps[1].serve.stderr`：准确已观察堆栈对 Rust 空 stderr。

其他 steps、tree/provider/history、退出码、signal 和 stream 事件仍参与普通比较。TS `--self` 不使用本 bug 映射，保留原不稳定结果。

正式 artifact 证明持久 partial、真实 signal/PID和恢复层；它没有 abort-RPC/socket-close 原帧。这部分由同 binary、可重验的独立八路 wire 证据证明。两层不能相互替代。

## 测试与实际边界

```sh
node --test scripts/bbx/sdk-signal-policy.test.mjs \
  scripts/bbx/sdk-signal-public.test.mjs \
  scripts/bbx/evidence.test.mjs scripts/bbx/check.test.mjs
```

单测中的 formal/wire 数据明确是 synthetic 合同。另一个 macOS 全链探针重验实际封存 binary 的 wire 原件，但 formal Rust 部分是 synthetic 克隆，证明比较/证据装置，不能冒充实际 Rust Gateway 恢复。Linux 最终候选仍须 E0 实际执行 Gateway/provider/持久化/resume。

## 已批准的唯一终端观察差异

2026-11-13用户在当前会话明确批准「批准，保留 Rust 完整终态（推荐）」。新条目`D1-SDK-SIGNAL-TERMINAL-OBSERVATION-001`独立于原abort-order崩溃差异；没有将本形态冒称持久数据丢失或固定测试矛盾。

具名scene新增`sdkSignalTerminalObservation`声明。它只认领第1 serve的stdout：TS准确六事件seq0/1/2/3/4/6、唯一缺result seq7，六事件值与Rust一致；原始持久registry有同run准确seq8 terminal。Rust必须完整六事件+准确result seq7，两侧signal/partial/canonical/恢复及同binary raw8wire仍全部强制。TS omission形态只许原SQLite warning，不接受任意close栈；新策略不认领stderr。旧abort-order策略也不认领新遗漏差异，各自独立打印ID。原环境变量与受信driver/verifier不变。

本次真实2c9首formal INVALID完整保留；`_tmp/20261113-sdk-terminal-proposal/public-first-rule-check/`为旧行为原件的新规则校验副本，仅重绑批准声明、本地严格重验wire身份和现有assertion记录，原stdout/stderr及行为字段没有改动，不是新正式产品执行。公共diff新规则exit0、仅新ID认领stdout；四个精准副本破坏（错run/缺其他事件/缺durable/Rust漏terminal）均INVALID2；当前TS与此前3fTS的self仍DIFF1且无新差异豁免。原artifact SHA前后不变。新正式pair由E0另行执行，不能覆盖或重算首成绩冒称原PASS。
