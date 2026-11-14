# 既有网页桥接的交互重连补丁

本目录是固定基线 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的只读运行时 overlay，验证环境为 **Node 22.22.0**。它没有修改基线文件，也不改变 Rust gateway 的协议和 owner 检查。

`GatewayWsClient` 在通知 disconnect observer 前清除 hello。原 bridge 随后读不到旧 binding，跳过 `reconnect_interaction`；新 WS 对旧 pending question/permission 的回答因此正确返回 `delivered:false`。补丁在每个 gateway 对象首次连接时保存服务端签发的不可变 binding，供 invalidate 和旧 stream catch 使用；仍由既有 `reconnect_interaction(previousBinding)` 和服务端注入的新 binding 完成合法迁移。它还阻止旧 reconcile Promise 清掉新一代 retirement。未伪造 connectionId、generation 或 nextBinding。

`index.js` 的两个回答分支仅在既有 helper 返回 `true` 时广播原有 `permission_cancelled` 确认，`false` 保留待答状态。前端等待该确认的配套修改由父目录的前端候选承担。

## 加载

保留既有 `tsx`，在它后面增加本目录 `register.mjs` 的绝对路径，再接基线 `ui/server/index.js`。例如在仓库根目录准备变量：

```sh
TASK_ROOT=/absolute/path/to/isolated/task
BASELINE_ROOT=/absolute/path/to/fixed/PilotDeck
RUNTIME_ROOT=/absolute/path/to/this/repository/scripts/backend-web-integration/runtime
```

由唯一 controller owner 在基线 `ui/` 目录启动，沿用其现有 sandbox 和隔离环境；以下列出需要明确保留的环境和 argv：

```sh
env HOME="$TASK_ROOT/home" PILOT_HOME="$TASK_ROOT/home/.pilotdeck" \
  TMPDIR="$TASK_ROOT/tmp" \
  PILOTDECK_WEB_BASELINE_ROOT="$BASELINE_ROOT" \
  PILOTDECK_WEB_RUNTIME_RECEIPT="$TASK_ROOT/ui-runtime.jsonl" \
  PILOTDECK_GATEWAY_URL="ws://127.0.0.1:29402/ws" \
  PILOTDECK_GATEWAY_TOKEN_PATH="$TASK_ROOT/home/.pilotdeck/server-token" \
  DATABASE_PATH="$TASK_ROOT/home/.pilotdeck/auth.db" \
  HOST=127.0.0.1 SERVER_PORT=29403 NODE_ENV=production \
  PILOTDECK_SKIP_BROWSER_OPEN=1 \
  /absolute/path/to/node22.22.0 --import tsx \
  --import "$RUNTIME_ROOT/register.mjs" "$BASELINE_ROOT/ui/server/index.js"
```

这段命令不替代 controller 的 Seatbelt、provider、组件路径、前端静态目录等既有隔离配置。不要直接运行它绕过 controller。`PILOTDECK_WEB_BASELINE_ROOT` 必须是绝对路径；receipt 路径在设置时也必须是绝对路径，父目录必须已存在并获 sandbox 写权限。

Loader 只匹配下表三个精确文件 URL。原始源码和变换结果分别与 `baseline-identity.json` 的完整 SHA256 核对；任意不匹配均拒绝加载。其他模块按原 loader 链处理。实际 UI 启动必须在 stderr 看见 **三条** `[web-reconnect-overlay]`，receipt JSONL 也须出现三个目标，分别核对：

| 目标 | 原始 SHA256 | 结果 SHA256 |
|---|---|---|
| `ui/server/pilotdeck-bridge.js` | `3d622cc4684f89ca565f20e7453750937ea4ad796c15d0e8f516fd69d5f5d3e2` | `a30852ee9e308162fc332acb9d571ee863263a14666b0f2edf9940b4d42d7f04` |
| `ui/server/index.js` | `ec679926da46a908d1e501c0b9bb84f949abbf0d67adb50e699dd069ddfdd40b` | `17d668ad0ffbc0111599a99feea829df01733078048173310e0b93080c530f18` |
| `ui/server/services/modelConnectionProbe.js` | `feb25d6cd28a73c2b45c8cfad524241bece91bd10dbda1b7dda0c680967fc32f` | `a9d1797c4cf7f5b7af025d5fd30425feccc2db4d1612a20cfdc5c385970ea79d` |

## 轻量控制

```sh
/absolute/path/to/node22.22.0 "$RUNTIME_ROOT/verify.mjs" \
  --baseline-root "$BASELINE_ROOT" --output "$TASK_ROOT/runtime-controls-new"
```

输出目录必须全新。入口验证固定基线完整 commit、隔离 HOME/PILOT_HOME，执行 11 个交互控制和 7 组模型能力控制，并与 `tsx`、注册 hook 同时解析完整变换后的三份 ESM。交互控制使用真实 connection cache 和真实 bridge/index 函数片段，fake Gateway 模拟 hello 先清空再通知、精确 previous binding 迁移、旧 observer、跨代 Promise 与两种 delivered 确认；包含删除 snapshot 和原版假取消的反例。模型能力控制通过真实 loader 导入目标 probe，使用本地内存 fake fetch，不发 API。解析检查不会链接或执行 UI 入口，不能证明实际 UI 入口已经经过 hook。真实启动的三条 loader 收据和网页恢复仍由 root 验收。

## 07：中文 text-only 能力诊断

这是保留的 TS UI 桥接实现的 bugfix，不能记为 Rust provider 迁移成果。智谱真实图像探测 HTTP400 已返回 `messages.content.type 参数非法，取值范围 ['text']`，原英文识别器误把明确 text-only 结果分类为 Undetermined。

新增识别要求 HTTP400、完整字段名 `messages.content.type`、`参数非法`、`取值范围`、且枚举只有一个 `text`。仅容许有限空白、英文/中文逗号和冒号、单/双引号格式。其它字段、包含 image/unknown、额外限流信息、401/403/429/500 不按新规则解释为不支持图像。原英文判定不改。

只给 `looksLikeImageUnsupported` 增加该规则并在现有 image 分支传入真实 HTTP status；原 text→image 顺序、默认4096输出预算、图像构造、协议和 catalog 均不动。mock控制核 text成功后image400得到 unsupported，text失败及未知400仍保留错误。

补丁只影响之后执行的 probe；已生成的 task result 不会自动重算。不要把内存旧 Undetermined 记录改名后宣称真实能力验证通过。已有真实400无需为了这条规则重复付费：可由 root 用该录得诊断做隔离 mock replay，另行核查 UI 显示与路由结果转换。

## 11：已证未执行的失败队列尝试在 Continue 后使用新 runId

固定 TS 的真实队列实现存在一个恢复缺口：`dispatchNextQueuedInput` 使用 `item.runId || item.id`，而显式 Continue 只解除暂停。某次尝试在会话恢复时失败后，即使坏历史已经离线修复，Continue 仍带原 runId。Gateway 的 RunRegistry 按既有幂等合同回放原失败，提前返回而不再读取修好的会话。不能通过清错误卡、改队列项 ID、删除 registry 或全局绕过幂等来解决。

此增量只接现有 `resume-input-queue` 操作。`queue-retry.mjs` 是本仓库自有实现；`queue-retry-overlay.mjs` 将它内联到 bridge 的隔离 IIFE 中，复用原 bridge 的 `randomUUID`，没有私有绝对路径 import。`baseline-identity.json` 同时固定 helper、transform、原07桥接结果及新结果 SHA；原交互恢复增量先按 `d48019ab…` 验证，再叠队列增量。index 与 model probe 的源/结果 SHA 保持原值。

新尝试仅在显式 Continue 且证据完整时分配：权威 activity inactive；同 session/project/run 的稳定 failed/gateway_submit_failed record；run_events 从0分页到 lastSeq、连续且仅有错误/状态事件；完整 native archive 无该 attempt 的接受/执行事件；前后 manifest 与 run record 身份稳定；本地 activityRevision、queueRevision、首项对象及内容未变化。原生 fork 日志可合法跳 sequence，因此 native archive 按严格递增、总数、首尾与 complete 核完整性。已有 Gateway native archive 接口承担存储访问，不增加协议；外部存储适配器沿其既有 replacement recovery 行为，不能概称所有实现都完全不写盘。

满足上述条件才为同一队列项分配新 `runId`，保持 `item.id`、正文、顺序、`acceptedInputIds` 与原 failed registry。队列仍暂停时先以 strict 模式同步写临时 sidecar 并原子 rename，成功后才调用原 resume/派发；这是进程恢复与写入顺序保证，不宣称 fsync 或断电持久性。持久化失败恢复内存旧 runId/revision 并保留暂停。同 session 的并发 Continue 有单一准备门，不能分配两个新尝试。已接受、active、未知交付、缺 API、页不全或竞态都不能获得新 runId。

明确的 ordinary queued + 权威 run_get 不存在记录是普通未登记发送，沿原 runId 调原 resume；它不生成新身份，也不绕过并发时可能出现的幂等记录。查询异常、未知状态和无法确认的交付不属于该例外。

便携受影响控制入口（只使用基线已有 Vitest；cache、HOME、sidecar 与原始输出全落新任务目录；envDir=false）：

```sh
/absolute/path/to/node22.22.0 "$RUNTIME_ROOT/verify-queue-retry.mjs" \
  --baseline-root "$BASELINE_ROOT" --output "$TASK_ROOT/queue-controls-new"
```

该入口固定基线 commit，要求恰好21项、零失败、零 pending，保存 JSON reporter 和原始 stdout/stderr。控制包括真实固定 TS public QueueManager 函数与真实 sidecar 的失败→修复→Continue→两项顺序派发，断言新身份已在首次派发前写入、原 registry 不变、原 item IDs/正文/顺序不变；其余定向负控覆盖已接受/active/unknown、完整分页、native 合法 sequence gap、权威与本地竞争、严格持久化失败和并发 Continue。Gateway registry/history/activity 为本地 mock，不构成实际 Rust admission 或网页实操通过。

2026-11-14 正式投影控制21/21、原桥接交互恢复控制11/11、三个完整变换 ESM 解析通过；原件由本轮交接引用。固定 TS 原版真实 queue oracle 已复现旧 runId 失败回放，最初两个装置失败（缺 Vitest 入口与 mock API 配置导致0 test）均保留，不计通过。实际修好历史后的网页 Continue 仍由 root 对同一冻结候选另验；此增量不改变 Rust61、前端 React bundle或既有鉴权/权限协议。

## 边界

- 内存快照仅用于同一 UI 进程中的 WS 短断连。UI 进程重启后，已丢失的旧 owner 证明不能靠新 hello 恢复。
- 跨代保护只保证旧 reconcile 不抹新 retirement；未声称解决多代部分成功、回执丢失或无限重试。
- Rust 的旧 binding、错误 session、错误 request 拒绝逻辑保持原样；轻量 fake Gateway 控制不构成 Rust 鉴权的新增实测。
- `delivered:false` 不得被归一化为成功，也不得移除待答提示。最终必须用真实网页验证 question 与 permission 在切 proxy 后可继续，且停机、后续轮和落盘正确。

## 会话归属隔离增量

此增量修复两条已独立复现的 UI WS 归属错误。权限和问题响应必须显式指定来源会话，只向该会话调用 Gateway，不再遍历其他 owner。`regenerate-last-message` 在 watch 或准备替换之前拒绝顶层、`options.sessionId`、`options.sessionKey` 的矛盾身份。仅顶层、仅 options 的合法旧入口仍沿用原事务，并将执行会话归一为同一 owner。接受、回滚、provider 失败及持久历史的其他行为保持固定基线合同。

前一 Web12 bridge/index 结果 SHA 固定在 `ownerIsolation`；原基线及最终变换 SHA 分别校验，loader 目标集合不变。控制实际执行变换后的公开 helper 和 WS 分支，覆盖缺失、未知、错误归属，以及合法旧编辑入口；每项已证失败以原基线为负控。这些控制不能代替修复后实际 WS/provider/transcript 验证或 root CUA 编辑。
