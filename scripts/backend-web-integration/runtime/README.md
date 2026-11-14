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

Loader 只匹配这两个精确文件 URL。两份原始源码和变换结果分别与 `baseline-identity.json` 的完整 SHA256 核对；任意不匹配均拒绝加载。其他模块按原 loader 链处理。实际 UI 启动必须在 stderr 看见 **两条** `[web-reconnect-overlay]`，receipt JSONL 也须出现两个目标，分别核对：

| 目标 | 原始 SHA256 | 结果 SHA256 |
|---|---|---|
| `ui/server/pilotdeck-bridge.js` | `3d622cc4684f89ca565f20e7453750937ea4ad796c15d0e8f516fd69d5f5d3e2` | `d48019ab042dec638d951817822ed90bc6f75076d551d4b1f0491cb5f5cfdfd7` |
| `ui/server/index.js` | `ec679926da46a908d1e501c0b9bb84f949abbf0d67adb50e699dd069ddfdd40b` | `17d668ad0ffbc0111599a99feea829df01733078048173310e0b93080c530f18` |

## 轻量控制

```sh
/absolute/path/to/node22.22.0 "$RUNTIME_ROOT/verify.mjs" \
  --baseline-root "$BASELINE_ROOT" --output "$TASK_ROOT/runtime-controls-new"
```

输出目录必须全新。入口验证固定基线完整 commit、隔离 HOME/PILOT_HOME，执行 11 个控制，并与 `tsx`、注册 hook 同时解析完整变换后的两份 ESM。控制使用真实 connection cache 和真实 bridge/index 函数片段，fake Gateway 模拟 hello 先清空再通知、精确 previous binding 迁移、旧 observer、跨代 Promise 与两种 delivered 确认；包含删除 snapshot 和原版假取消的反例。解析检查不会链接或执行 UI 模块，不能证明实际 UI 入口已经经过 hook。真实启动的两条 loader 收据和网页恢复仍由 root 验收。

## 边界

- 内存快照仅用于同一 UI 进程中的 WS 短断连。UI 进程重启后，已丢失的旧 owner 证明不能靠新 hello 恢复。
- 跨代保护只保证旧 reconcile 不抹新 retirement；未声称解决多代部分成功、回执丢失或无限重试。
- Rust 的旧 binding、错误 session、错误 request 拒绝逻辑保持原样；轻量 fake Gateway 控制不构成 Rust 鉴权的新增实测。
- `delivered:false` 不得被归一化为成功，也不得移除待答提示。最终必须用真实网页验证 question 与 permission 在切 proxy 后可继续，且停机、后续轮和落盘正确。
