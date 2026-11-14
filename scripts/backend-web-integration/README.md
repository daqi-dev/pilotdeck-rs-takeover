# 固定基线网页的后端集成修补

本目录把已经实际构建、用于网页交互验证的七文件 overlay 保存为可重新执行的构建入口。固定参考为 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`；基线源码、已有依赖和 public 资源只读解析，所有差额在 Vite transform 阶段应用，不修改或复制整套基线实现。

这里保留 **TS React 网页** 和 **TS `ui/server`**：React 提供现有业务界面，Express UI server 提供网页 REST/WS、文件预览等本地能力以及浏览器协议到 Rust gateway RPC 的桥接。Rust 后端负责 gateway/native agent。该组合只修复当前 Web 集成实际遇到的缺陷，不代表完整前端迁移、Rust UI server 交付或 TS UI 的退役；StaffDeck 不在本项范围内。

## 固定差额

`patches.json` 与 2026-11-14 已构建的 `frontend-candidate-02` 输入逐字一致，SHA256 为 `2e4811be00fbfa0fe664e39d684a7e542e7087478a9a0b8060df130c4db957fe`。每项同时登记固定源码 SHA256、唯一替换锚点和结果 SHA256，任一不符就失败。

| 差额 | 文件 | 改变 |
| --- | --- | --- |
| no-login WebSocket | `components/auth/types.ts`、`components/auth/context/AuthContext.tsx`、`contexts/WebSocketContext.tsx` | 由成功的服务端 auth/status 响应公开 authDisabled；仅明确关闭本地认证时允许同 origin 无 token WS，要求认证时保留 token gate；状态变化清理旧 socket |
| 附件临时租约展示 | `components/chat/utils/attachmentNotes.ts`、`components/chat-v2/messageFileCardUtils.ts`、`stores/useSessionStore.ts`、`components/chat/hooks/useUploadedAttachmentPreviews.ts` | 临时 chat-upload-leases 路径恢复为上传标识，以 uploadId/attachmentId 去重；不把租约当永久工作区文件；通过已有上传元数据接口恢复文件名/MIME/大小 |

表中路径相对基线 `ui/src/`。不伪造认证 token、不放宽服务器鉴权、不延长上传保留期、不把临时路径变成持久下载 URL。

## 运行

需要已有的固定基线 Git checkout、与其锁文件匹配的已安装依赖、Node `>=22.13.0 <23`。此脚本不安装依赖、不下载二进制、不启动服务。Git HEAD、tracked clean、七文件 source/result SHA、Vite 配置、package manifests 和 pnpm lock 的 SHA 都在构建前校验。`baseline.json` 是固定输入身份，不能为了兼容其他上游版本而跳过检查。

```sh
node scripts/backend-web-integration/build.mjs \
  --baseline /absolute/read-only/PilotDeck \
  --out /absolute/task/_tmp/NEW-web-build
```

两个参数都必须是绝对路径；输出父目录须已存在，输出目录必须不存在。输出不能与基线重叠；位于本仓库内部时，只允许 `_tmp/` 或 `_build/`，禁止写 `resources/`、产品源码或脚本目录。不会覆盖第一次失败或成功的构建；重试必须选择新的输出目录。

脚本清空继承环境，仅保留 PATH，并在输出目录下创建隔离 `home/`、`pilot-home/`、`tmp/`、`cache/`；固定 production、UTC 与非 platform 的 no-login 编译设置。原基线 Vite 配置的运行副本保留原有 React/CodeMirror alias、chunks、appearance 等设置，但剔除 `loadEnv` 调用，另设 `configFile:false` 和 `envDir:false`，不读取基线 `.env`。tsx/jiti 磁盘缓存关闭。原配置副本、Vite 输出、运行清单和结果均留在输出目录。

产物包括：

- `dist/`：候选网页，可由任务自己的静态代理服务托管，将 `/api/*` 与 `/ws` 转发既有隔离 TS UI server。
- `source-manifest.json`：基线提交、七个 source/result SHA、其他固定输入、构建脚本身份、Node 版本和命令。
- `build-result.json`：明确 built/failed；成功同时记录 index.html SHA256 和七文件名单。
- `baseline-config.mjs`：只用于本次构建的原配置运行副本，保留其基线绝对定位，不能拿它当跨机器发布配置。

构建末尾再次核对基线与 overlay 输入，发现漂移则报告失败。输出清单中的机器路径是构建证据，不是网页运行时的源代码定位。已安装 node_modules 的平台产物仍是外部构建输入；这里不承诺未经验证的跨机器字节一致。

## 验证口径与当前边界

七文件差额来自同一份实际通过构建的 candidate-02；正式目录只整理参数、隔离输出和身份校验，不改替换语义。正式化后的构建入口尚未重新执行完整构建，不能把旧候选的构建成功冒称为这个入口已构建通过。这里只做语法与输入对应核验，不添加重复测试。

网页验证须在隔离实例上分别保存：无 token 的 no-login 实际会话、auth-required 新 origin 的注册/登录 gate 及服务端无 token 握手拒绝，以及附件展示/预览/刷新后恢复。完整六条网页路径和少量真实模型任务由本轮集成证据记录；脚本自身、构建成功或函数测试不能代替这些真实操作。此脚本不启动、重启或管理任何 UI/gateway/provider 服务。
