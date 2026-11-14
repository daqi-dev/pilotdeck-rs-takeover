# 固定基线网页的后端集成修补

本目录把已经实际构建、用于网页交互验证的九文件 overlay 保存为可重新执行的构建入口。固定参考为 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`；基线源码、已有依赖和 public 资源只读解析，所有差额在 Vite transform 阶段应用，不修改或复制整套基线实现。

这里保留 **TS React 网页** 和 **TS `ui/server`**：React 提供现有业务界面，Express UI server 提供网页 REST/WS、文件预览等本地能力以及浏览器协议到 Rust gateway RPC 的桥接。Rust 后端负责 gateway/native agent。该组合只修复当前 Web 集成实际遇到的缺陷，不代表完整前端迁移、Rust UI server 交付或 TS UI 的退役；StaffDeck 不在本项范围内。

## 固定差额

`patches.json` 的前七个对象及其全部字节字段保持已构建 `frontend-candidate-02` / 正式 03 的输入不变；原七项清单 SHA256 为 `2e4811be00fbfa0fe664e39d684a7e542e7087478a9a0b8060df130c4db957fe`。本次只在末尾追加两个恢复修补对象；完整九项清单 SHA256 为 `7d364e3b914cfb28bd5fd3052d7770bf07fb2fa3001cedde5ca4b65199245234`。每项同时登记固定源码 SHA256、唯一替换锚点和结果 SHA256，任一不符就失败。

| 差额 | 文件 | 改变 |
| --- | --- | --- |
| no-login WebSocket | `components/auth/types.ts`、`components/auth/context/AuthContext.tsx`、`contexts/WebSocketContext.tsx` | 由成功的服务端 auth/status 响应公开 authDisabled；仅明确关闭本地认证时允许同 origin 无 token WS，要求认证时保留 token gate；状态变化清理旧 socket |
| 附件临时租约展示 | `components/chat/utils/attachmentNotes.ts`、`components/chat-v2/messageFileCardUtils.ts`、`stores/useSessionStore.ts`、`components/chat/hooks/useUploadedAttachmentPreviews.ts` | 临时 chat-upload-leases 路径恢复为上传标识，以 uploadId/attachmentId 去重；不把租约当永久工作区文件；通过已有上传元数据接口恢复文件名/MIME/大小 |

| 断连后残留交互 | `components/chat/types/types.ts`、`components/chat/hooks/useChatRealtimeHandlers.ts` | 保存授权/问题所属 runId；传输错误触发已有状态查询，只有匹配请求序号和预期 run 的权威 inactive 回复才清理该 session/run；明确终态和取消同样限定身份 |

表中路径相对基线 `ui/src/`。不伪造认证 token、不放宽服务器鉴权、不延长上传保留期、不把临时路径变成持久下载 URL。

## 运行

需要已有的固定基线 Git checkout、与其锁文件匹配的已安装依赖、Node `>=22.13.0 <23`。此脚本不安装依赖、不下载二进制、不启动服务。Git HEAD、tracked clean、九文件 source/result SHA、Vite 配置、package manifests 和 pnpm lock 的 SHA 都在构建前校验。`baseline.json` 是固定输入身份，不能为了兼容其他上游版本而跳过检查。

```sh
node scripts/backend-web-integration/build.mjs \
  --baseline /absolute/read-only/PilotDeck \
  --out /absolute/task/_tmp/NEW-web-build
```

两个参数都必须是绝对路径；输出父目录须已存在，输出目录必须不存在。输出不能与基线重叠；位于本仓库内部时，只允许 `_tmp/` 或 `_build/`，禁止写 `resources/`、产品源码或脚本目录。不会覆盖第一次失败或成功的构建；重试必须选择新的输出目录。

脚本清空继承环境，仅保留 PATH，并在输出目录下创建隔离 `home/`、`pilot-home/`、`tmp/`、`cache/`；固定 production、UTC 与非 platform 的 no-login 编译设置。原基线 Vite 配置的运行副本保留原有 React/CodeMirror alias、chunks、appearance 等设置，但剔除 `loadEnv` 调用，另设 `configFile:false` 和 `envDir:false`，不读取基线 `.env`。tsx/jiti 磁盘缓存关闭。原配置副本、Vite 输出、运行清单和结果均留在输出目录。

产物包括：

- `dist/`：候选网页，可由任务自己的静态代理服务托管，将 `/api/*` 与 `/ws` 转发既有隔离 TS UI server。
- `source-manifest.json`：基线提交、九个 source/result SHA、其他固定输入、构建脚本身份、Node 版本和命令。
- `build-result.json`：明确 built/failed；成功同时记录 index.html SHA256 和九文件名单。
- `baseline-config.mjs`：只用于本次构建的原配置运行副本，保留其基线绝对定位，不能拿它当跨机器发布配置。

构建末尾再次核对基线与 overlay 输入，发现漂移则报告失败。输出清单中的机器路径是构建证据，不是网页运行时的源代码定位。已安装 node_modules 的平台产物仍是外部构建输入；这里不承诺未经验证的跨机器字节一致。

## 验证口径与当前边界

前七文件差额已在 candidate-02 及正式 03 实际构建通过；正式 03 的 index.html SHA256 为 `2d46165b13d80aad491291216a0290c8b653de2726c207baa74bcfc84b70d9a2`。新增两文件恢复差额及完整九项候选 04 尚待构建和网页复验；不把 03 的成功写成 04 已通过。

网页验证须在隔离实例上分别保存：无 token 的 no-login 实际会话、auth-required 新 origin 的注册/登录 gate 及服务端无 token 握手拒绝，以及附件展示/预览/刷新后恢复。完整六条网页路径和少量真实模型任务由本轮集成证据记录；脚本自身、构建成功或函数测试不能代替这些真实操作。此脚本不启动、重启或管理任何 UI/gateway/provider 服务。


## 断连交互恢复的窄修补

58 后端在 pending permission / question 时退出并重启，原浏览器到 UI bridge 的 WS 仍在线，页面收到合成 `gateway_unavailable` 错误，但没有对应的最终取消帧。旧前端立刻丢弃 activeRunId，且 error / inactive status 分支没有移除 pending 交互，因此授权面板和问题模态残留；手动刷新可消失。原始故障、首次失败和实测图像保留在本轮私有证据，不能用后续通过覆盖。

单纯传输错误不能证明取消：短断连可能保留合法 pending。本修补在终态判断入口把没有 runId 的 `gateway_unavailable` 转为原有 synchronizing 状态，保留当前 run，调用已有 `check-session-status`；没有新 API。unknown/null 继续原有重试，active=true 保留合法交互。只有通过原有 statusRequestId / expectedActiveRunId 防陈旧检查、具有有效序号、明确 isProcessing=false 时，才按 sessionId 与 runId 同时匹配删除旧交互。别的 session、新 run、无身份的未决项不因这类传输错误被清空。

明确 complete / terminal error 只清自身 run；明确 permission_cancelled 使用 sessionId/requestId，双方都有 runId 时还必须相同。兼容旧无 runId complete 仅在当前没有已知 active run 时清同 session 的无 runId 旧项，不清任何已知 run 项。permission 与 question 共用同一 pending 模型，isElicitation 保持原协议。

没有修复工具摘要把未执行 write_file 展示为 Edited 1 file 的独立展示问题；它不作为恢复验证通过条件。

## 必要的 11 项恢复回归

自有 `recovery.test.ts` 通过别名读取固定基线 hook 并施加相同 overlay，不复制基线实现。入口 `check-recovery.mjs` 使用既有依赖、单 worker、任务隔离缓存，不启动产品服务、不发 provider 请求、不读取基线 .env，也不扩展鉴权或恢复测试矩阵。

```sh
node scripts/backend-web-integration/check-recovery.mjs \
  --baseline /absolute/read-only/PilotDeck \
  --out /absolute/task/_tmp/NEW-recovery-check
```

输出父目录必须存在；输出目录必须不存在，沿用构建入口的源码/基线保护边界。结果写入输出目录 `results.json`，进程退出码及 stdout 应由运行者保留。检查覆盖 permission/question 各自精确 run 恢复、unknown 与 active 保留、错误序号、旧 run 回复对新 run 的保护、qualified complete/error、明确取消范围以及两种窄 legacy complete 兼容，总计 11 项。

原私有 checks-04 已实际 11/11 PASS，并经独立审查；正式便携入口 recovery-formal-check-01 也已实际 11/11 PASS（2026-11-14），没有运行新网页构建或启动产品。此前 checks-01 因外部测试根目录配置失败执行 0 项，checks-03 虽 PASS 但 active 保留控制只推进 1000ms、未达到生产 1200ms 重试，不能算该分支证据。checks-04 改为推进 1300ms，并断言新请求序号增加、runtime 从 synchronizing 变为 running，确认 active=true 回复确被接受。这些原记录均保留。
