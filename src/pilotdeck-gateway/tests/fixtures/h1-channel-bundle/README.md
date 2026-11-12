# H1 ChannelAdapterBundle 重载特征证据

固定参考为 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，Node 必须为 `v22.22.0`。`capture.mjs` 是本仓库独立编写的驱动，不含参考实现源码。`metadata.json` 固定 Bundle、生命周期、loader 和 Signal / Mattermost / HomeAssistant 的 12 个源码与构建文件 SHA-256；同时检查 Git HEAD 和参考工作树的 `src/`、`packages/` 没有已跟踪修改。额外固定 TypeScript 5.9.3 编译器及 tsconfig 哈希，逐一将 6 个参考源码在内存转译并与现有 dist 逐字节比较；因参考包为 ESM，单文件转译显式选择 ESNext emit，其余选项来自原 tsconfig。没有写入或重建参考目录。

证据层级是 **模块组合 + mock**：直接调用基线真实 `ChannelAdapterBundle`、`loadEnabledChannels`、`startPilotDeckServer` 生命周期模块；真实 loader 构造三个渠道类，随后用受控 start/stop 替代渠道网络行为。Gateway 使用空 mock；没有从正式 CLI / server 命令进入，也没有执行真实配置重载 caller。此证据证明 Bundle 的行为，不能替代 H1 正式 server 入口验证。

## 复跑

在 Rust checkout 根目录执行，预先准备只读的固定基线与其 `dist/`。默认基线路径为 `/Users/da/ws/pilotdeck-rs/resources/PilotDeck`；其他机器可在命令前设置 `PDRS_BASELINE=/absolute/read-only/reference`。驱动不安装依赖、不构建参考、不调用 Cargo。

```sh
mkdir -p _tmp/20261113-h1-bundle
node src/pilotdeck-gateway/tests/fixtures/h1-channel-bundle/capture.mjs _tmp/20261113-h1-bundle/run-1
node src/pilotdeck-gateway/tests/fixtures/h1-channel-bundle/capture.mjs _tmp/20261113-h1-bundle/run-2
node src/pilotdeck-gateway/tests/fixtures/h1-channel-bundle/capture.mjs --compare _tmp/20261113-h1-bundle/run-1/summary.json _tmp/20261113-h1-bundle/run-2/summary.json
# 下条必须 exit 1；检查 mutant-1/failure.json 的 phase 为 verify。
node src/pilotdeck-gateway/tests/fixtures/h1-channel-bundle/capture.mjs --mutant-drop-history _tmp/20261113-h1-bundle/mutant-1
```

输出目录必须不存在；文件以 `wx` 写入，复跑换新目录，已有失败产物保留。在动态加载基线前，驱动清空继承环境，将 HOME、PILOT_HOME、TMPDIR 指向输出目录的独立子目录。只打开 `127.0.0.1:0` 随机 HTTP 监听，不启动子进程、模型或渠道网络连接。配置中的 `127.0.0.1:1` 是不会被访问的构造器测试值。唯一生命周期实例在 `finally` 中关闭；不使用固定 18789，也不终止其他进程。整个采集有 15 秒进程内截止时间；超时仅退出本驱动自身进程（exit 124，OS 释放其随机监听），并写 `deadline.json`。未捕获错误写 `failure.json`，含当前阶段、原错误和已观察轨迹；原先记录的首错不会被清理错误覆盖。

每轮写入 `observed.json` 和 `summary.json`。前者在断言前落盘，便于保留首个结果不符；后者包含完整参考版本、Node 版本、源码/构建/驱动哈希、结果、构造选项、reconcile 输入、start/stop 顺序和负控结果。双跑比较要求身份和完整观察值相等，不归一化业务字段。Node 自带 SQLite experimental warning 和受控替换失败时的恢复日志可出现在终端；这些终端日志不作为此模块证据的计分字段。

## 固定行为与负控

11 个操作包含 startup、替换/新增/禁用、全部禁用、手工重启旧 key 后再次禁用、恢复基线实例、候选 start 失败、禁用恢复的实例、启动 stop 会失败的实例、stop 失败、重试禁用、loader seam 拒绝。它们产生 9 次 reconcile。

- 初始 managed key 为 `feishu, weixin, qq, wecom`。startup 的配置对象故意先写 HomeAssistant，再写 Signal；实际构造与加入历史的顺序仍为 `signal, homeassistant`。之后新增 Mattermost，历史尾部追加 `mattermost`，禁用后不删除。
- 重载 desired 顺序为 `mattermost, signal`，先以 `config-disabled` 停止旧 HomeAssistant，再启动 Mattermost，并以 `hot-reload` 替换 Signal。返回值按所有 started、然后所有 stopped 排列。
- 构造器获得重载配置的新 token、URL、account、teamId；所有 `start(deps).config` 仍是初始 snapshot。这是基线行为。
- 候选 start 拒绝时旧实例会重新启动，但 reload 仍以原错误拒绝；stop 拒绝时旧 handle 被保留，下一次禁用重试成功。注入的 loader seam 拒绝会在 reconcile 前拒绝；这不等同于 loader 内部单个 constructor 抛错，后者按基线自身逻辑记录后继续。
- 每轮对已经观察的结果 JSON 中的 loader 顺序、历史 key、构造配置、停止理由和失败返回值分别注入一处错误，共 5 个比较器自测；全部必须被拒绝。这些属于比较器负控，不宣称为产品代码变异。另用 `--mutant-drop-history` 在第三次实际 reconcile 前移除传入生命周期的历史 signal key；真实 TS server 因而未停掉手工重启的 signal，`disable-again-after-manual-start` 返回空列表。正常行为断言必须报错 exit 1。这是模块装配边界输入变异，也不是正式 server 产品代码变异。

Rust 对应的 `native_process/channels_h1_tests.rs` 通过 `cfg(test)` 的 `new_with_loader_mounts` 驱动真实 NativeChannels / loader / ChannelServer，覆盖相同重载主链、初始与历史 key 保留、非 managed key 不被禁用，以及失败恢复与重试。它不修改生产 mount 或基线 oracle。编译和执行由根协调者统一安排；仅生成文件或通过 rustfmt 不代表测试已通过。

## 本轮记录

2026-11-13，本机 `validated-1` / `validated-2` 各通过 11 操作、9 reconcile、5 个比较器负控，双跑完整自比通过；`validated-mutant-1` 实际边界输入变异 exit 1 被拒绝。产物位于 `_tmp/20261113-h1-bundle/`，不入库。最终结果与源码身份以根协调者的 H1 交付记录为准。
