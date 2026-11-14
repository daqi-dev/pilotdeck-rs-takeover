# 固定基线网页的后端集成修补

本目录保存现有网页与后端联调所需的十三文件 overlay 和可重新执行的构建入口；此前十一文件候选已构建，新追加的路由修补尚待组合构建与页面验证。固定参考为 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`；基线源码、已有依赖和 public 资源只读解析，所有差额在 Vite transform 阶段应用，不修改或复制整套基线实现。

这里保留 **TS React 网页** 和 **TS `ui/server`**：React 提供现有业务界面，Express UI server 提供网页 REST/WS、文件预览等本地能力以及浏览器协议到 Rust gateway RPC 的桥接。Rust 后端负责 gateway/native agent。该组合只修复当前 Web 集成实际遇到的缺陷，不代表完整前端迁移、Rust UI server 交付或 TS UI 的退役；StaffDeck 不在本项范围内。

## 固定差额

`patches.json` 的前七个对象及其全部字节字段保持已构建 `frontend-candidate-02` / 正式 03 的输入不变；原七项清单 SHA256 为 `2e4811be00fbfa0fe664e39d684a7e542e7087478a9a0b8060df130c4db957fe`。04追加两个恢复修补对象；05仅更新其中realtime对象并追加composer对象，05十项清单SHA256为 `a0c6fe1b3039cb885fd86e8117a502890d87150f570467ff96ffa0ee75f418ed`；06仅追加ProviderCard对象，完整十一项清单SHA256为 `41b684b6bbcd6878739692033490745de0ef258319de7eb7fb6f3d5a8f24aba4`。Web11追加两项旧会话路由恢复差额，十三项清单SHA256为 `9f9dd338fce8f3ee59bea254c6bd29ef96a12232fe9586a9b09d16b5948121fb`；Web12 修复无 sessionId 时的 null 访问，当前清单SHA256为 `3b87f686bc184415d4b86753b288bf59eda3dd43c48c52087ed947fed4c8598b`，新组合尚未构建或实页验证。每项同时登记固定源码 SHA256、唯一替换锚点和结果 SHA256，任一不符就失败。

| 差额 | 文件 | 改变 |
| --- | --- | --- |
| no-login WebSocket | `components/auth/types.ts`、`components/auth/context/AuthContext.tsx`、`contexts/WebSocketContext.tsx` | 由成功的服务端 auth/status 响应公开 authDisabled；仅明确关闭本地认证时允许同 origin 无 token WS，要求认证时保留 token gate；状态变化清理旧 socket |
| 附件临时租约展示 | `components/chat/utils/attachmentNotes.ts`、`components/chat-v2/messageFileCardUtils.ts`、`stores/useSessionStore.ts`、`components/chat/hooks/useUploadedAttachmentPreviews.ts` | 临时 chat-upload-leases 路径恢复为上传标识，以 uploadId/attachmentId 去重；不把租约当永久工作区文件；通过已有上传元数据接口恢复文件名/MIME/大小 |

| 预设provider已有URL编辑 | `components/settings/view/modelPool/components/ProviderCard.tsx` | 预设也展示可编辑API Base URL，保留catalog默认和预设协议，保存与重读使用现有url schema |
| 答复回执与合法重连 | `components/chat/hooks/useChatComposerState.ts`，并更新 realtime handler | 答复后等待既有取消回执；相同session/request的未知与已知run合并保留known，冲突known不覆盖 |
| 断连后残留交互 | `components/chat/types/types.ts`、`components/chat/hooks/useChatRealtimeHandlers.ts` | 保存授权/问题所属 runId；传输错误触发已有状态查询，只有匹配请求序号和预期 run 的权威 inactive 回复才清理该 session/run；明确终态和取消同样限定身份 |
| 旧会话深链定位 | `hooks/useProjectsState.ts`、`components/app-shell/AppShellV2.tsx` | 首屏未包含路由会话时使用既有分页查找；未定位期间阻断聊天主区，失败明确提示；过期异步结果不可选中 |

表中路径相对基线 `ui/src/`。不伪造认证 token、不放宽服务器鉴权、不延长上传保留期、不把临时路径变成持久下载 URL。

## 运行

需要已有的固定基线 Git checkout、与其锁文件匹配的已安装依赖、Node `>=22.13.0 <23`。此脚本不安装依赖、不下载二进制、不启动服务。Git HEAD、tracked clean、十三文件 source/result SHA、Vite 配置、package manifests 和 pnpm lock 的 SHA 都在构建前校验。`baseline.json` 是固定输入身份，不能为了兼容其他上游版本而跳过检查。

```sh
node scripts/backend-web-integration/build.mjs \
  --baseline /absolute/read-only/PilotDeck \
  --out /absolute/task/_tmp/NEW-web-build
```

两个参数都必须是绝对路径；输出父目录须已存在，输出目录必须不存在。输出不能与基线重叠；位于本仓库内部时，只允许 `_tmp/` 或 `_build/`，禁止写 `resources/`、产品源码或脚本目录。不会覆盖第一次失败或成功的构建；重试必须选择新的输出目录。

脚本清空继承环境，仅保留 PATH，并在输出目录下创建隔离 `home/`、`pilot-home/`、`tmp/`、`cache/`；固定 production、UTC 与非 platform 的 no-login 编译设置。原基线 Vite 配置的运行副本保留原有 React/CodeMirror alias、chunks、appearance 等设置，但剔除 `loadEnv` 调用，另设 `configFile:false` 和 `envDir:false`，不读取基线 `.env`。tsx/jiti 磁盘缓存关闭。原配置副本、Vite 输出、运行清单和结果均留在输出目录。

产物包括：

- `dist/`：候选网页，可由任务自己的静态代理服务托管，将 `/api/*` 与 `/ws` 转发既有隔离 TS UI server。
- `source-manifest.json`：基线提交、十三个 source/result SHA、其他固定输入、构建脚本身份、Node 版本和命令。
- `build-result.json`：明确 built/failed；成功同时记录 index.html SHA256 和十三文件名单。
- `baseline-config.mjs`：只用于本次构建的原配置运行副本，保留其基线绝对定位，不能拿它当跨机器发布配置。

构建末尾再次核对基线与 overlay 输入，发现漂移则报告失败。输出清单中的机器路径是构建证据，不是网页运行时的源代码定位。已安装 node_modules 的平台产物仍是外部构建输入；这里不承诺未经验证的跨机器字节一致。

## 验证口径与当前边界

前七文件差额已在 candidate-02 及正式 03 实际构建通过；正式 03 的 index.html SHA256 为 `2d46165b13d80aad491291216a0290c8b653de2726c207baa74bcfc84b70d9a2`。正式04的九项候选也已构建，index.html SHA256为 `88ab2a609ed145f2d5b940fe380f8357c7f9ebb0d239be2455e9698a726c321b`。05新增答复回执/合法重连差额，完整十项候选尚待构建与网页复验；不把03/04的成功写成05已通过。

网页验证须在隔离实例上分别保存：无 token 的 no-login 实际会话、auth-required 新 origin 的注册/登录 gate 及服务端无 token 握手拒绝，以及附件展示/预览/刷新后恢复。完整六条网页路径和少量真实模型任务由本轮集成证据记录；脚本自身、构建成功或函数测试不能代替这些真实操作。此脚本不启动、重启或管理任何 UI/gateway/provider 服务。


## 断连交互恢复的窄修补

58 后端在 pending permission / question 时退出并重启，原浏览器到 UI bridge 的 WS 仍在线，页面收到合成 `gateway_unavailable` 错误，但没有对应的最终取消帧。旧前端立刻丢弃 activeRunId，且 error / inactive status 分支没有移除 pending 交互，因此授权面板和问题模态残留；手动刷新可消失。原始故障、首次失败和实测图像保留在本轮私有证据，不能用后续通过覆盖。

单纯传输错误不能证明取消：短断连可能保留合法 pending。本修补在终态判断入口把没有 runId 的 `gateway_unavailable` 转为原有 synchronizing 状态，保留当前 run，调用已有 `check-session-status`；没有新 API。unknown/null 继续原有重试，active=true 保留合法交互。只有通过原有 statusRequestId / expectedActiveRunId 防陈旧检查、具有有效序号、明确 isProcessing=false 时，才按 sessionId 与 runId 同时匹配删除旧交互。别的 session、新 run、无身份的未决项不因这类传输错误被清空。

明确 complete / terminal error 只清自身 run；明确 permission_cancelled 使用 sessionId/requestId，双方都有 runId 时还必须相同。兼容旧无 runId complete 仅在当前没有已知 active run 时清同 session 的无 runId 旧项，不清任何已知 run 项。permission 与 question 共用同一 pending 模型，isElicitation 保持原协议。

没有修复工具摘要把未执行 write_file 展示为 Edited 1 file 的独立展示问题；它不作为恢复验证通过条件。

## 必要的 19 项交互回归

自有 `recovery.test.ts` 通过别名读取固定基线 hook 并施加相同 overlay，不复制基线实现。入口 `check-recovery.mjs` 使用既有依赖、单 worker、任务隔离缓存，不启动产品服务、不发 provider 请求、不读取基线 .env，也不扩展鉴权或恢复测试矩阵。

```sh
node scripts/backend-web-integration/check-recovery.mjs \
  --baseline /absolute/read-only/PilotDeck \
  --out /absolute/task/_tmp/NEW-recovery-check
```

输出父目录必须存在；输出目录必须不存在，沿用构建入口的源码/基线保护边界。结果写入输出目录 `results.json`，进程退出码及 stdout 应由运行者保留。检查覆盖 permission/question 各自精确 run 恢复、unknown 与 active 保留、错误序号、旧 run 回复对新 run 的保护、qualified complete/error、明确取消范围以及两种窄 legacy complete 兼容，保留原11项；05追加8项窄控制，覆盖runless replay双向兼容与冲突保护、真实composer permission/question在sendfalse/true无ACK时保留、成功ACK后清理及别session/重复ACK不清Waiting，总计19项。

原私有 checks-04 已实际 11/11 PASS，并经独立审查；正式便携入口 recovery-formal-check-01 也已实际 11/11 PASS（2026-11-14），没有运行新网页构建或启动产品。此前 checks-01 因外部测试根目录配置失败执行 0 项，checks-03 虽 PASS 但 active 保留控制只推进 1000ms、未达到生产 1200ms 重试，不能算该分支证据。checks-04 改为推进 1300ms，并断言新请求序号增加、runtime 从 synchronizing 变为 running，确认 active=true 回复确被接受。这些原记录均保留。


## 05 答复与合法重连的边界

合法 bridge rebind 的 replay DTO 可能没有 runId。相同 sessionId/requestId 只要一方 run 未知就合并，保留已知 run；未知项收到已知 run 只补该字段；双方已知且不同则不合并、不覆盖。不能用重连补帧制造重复授权/问题卡。

调用 sendMessage 只是提交尝试。Composer 不再乐观移除面板或清Waiting状态；sendfalse、尚未ACK、服务端delivered:false必须保留。仍使用原有 permission-response/elicitation-response 与 permission_cancelled 回执，没有新协议。确实移除当前view最后项时才清Waiting状态，重复/别session的回执不能改变当前等待。

TS UI server 的“仅delivered:true广播既有取消回执”及合法bridge rebind修补由独立runtime交付负责，不属于这个前端overlay。前端19项控制与其组合后的05构建/真实网页恢复必须分别报告；此目录不声称前端控制通过已代替端到端交互验收。

05正式便携入口 `recovery-formal-check-05` 已实际19/19 PASS（2026-11-14），没有新build或服务操作；05网页候选仍待主协调者唯一构建及真实页面复验。


## 06 预设 provider 的已有 Base URL 能力

智谱等预设已经有 `model.providers.<id>.url` schema，但原Card把URL输入和custom协议选择一起隐藏。06只让预设和自定义provider都能编辑既有Base URL，预设协议仍不可在此改变，catalog默认与模型名单均不修改。省略url时输入显示原catalog默认；显式保存的非默认url继续优先。没有新增套餐自动探测或Coding Plan专用预设。

保存链已核对：ProviderCard保存effectiveUrl，ModelsSection原样patch provider，normalizePilotDeckConfig不会重写url，resolveConfiguredProviderUrl和parseModelConfig均优先显式url。固定源码中没有名为buildCatalogProvider的覆盖步骤；控制走真实ModelsSection/Card保存链，没有通过替换onSave绕过UI逻辑。

原安全边界保持：masked key与URL/protocol同时变更时，服务端要求重新输入key；06不会把旧key静默发送到新scope。用户变更地址需显式重新输入对应凭据。定点控制首次换URL时显式输入合成key；保存后重新读取mask，相同scope再次保存保持原key，证明mask往返不丢失。跨scope要求重输的保护仅由源码核验；这4项mock没有执行config route的scope validator，不冒称该路由负控已运行。没有读取用户key、真实配置或.env，也不调用真实provider。

```sh
node scripts/backend-web-integration/check-provider-settings.mjs \
  --baseline /absolute/read-only/PilotDeck \
  --out /absolute/task/_tmp/NEW-provider-settings-check
```

仅4项必要mock控制：预设非默认URL保存/重读/mask同scope再保存、预设默认回退及固定协议、无效URL拦截、自定义provider协议/URL输入兼容。实际controls-02为4/4 PASS；controls-01因新runner缺jsx-runtime alias执行0项，原记录保留，补齐alias后通过。该测试不证明sandbox外网可达；用户看到的fetch failed与预设地址隐藏是不同问题，不能把这次UI修补称为网络限制已解除。06独立dist已由本入口唯一构建完成（2026-11-14，14.03s，exit0），11项差额齐全；index.html SHA256为 `66660790f0506e6e8fbc4189d8f86438aecb56e18aff1a66714c95e50db0c16d`。构建有既有大chunk warning，没有编译错误。实际网页操作及真实provider请求仍未由本工作包执行。


## 旧会话深链恢复定点控制

原项目列表仅预览每项目最新五个会话；旧 `/session/:id` 不在预览时原 hook 不会查找后续页，可能显示空白。新增 fallback 使用既有分页接口，不向侧栏已加载前缀插入远页单项。取消过期查找，页数据异常、重复页、空页带 hasMore、超过200页、请求失败或多个项目同时匹配时拒绝猜测；未定位时 AppShell 不挂载可发送的聊天主区。原 sidebar 预览已命中的选择规则不变，不声称所有路径均完成跨项目唯一性检查。

```sh
node scripts/backend-web-integration/check-route-recovery.mjs \
  --baseline /absolute/read-only/PilotDeck \
  --out /absolute/task/_tmp/NEW-route-check
```

八项真实 React hook 控制覆盖远页会话定位且 sidebar 预览数量不变、缺失目标清除旧选择并报错、导航后丢弃过期请求、503 时不选会话，以及 project/General 无 sessionId 初始化和从既有路由切换到新会话。工具检查完整 overlay 与固定基线身份，在新建隔离输出目录写 `results.json`；不启动产品服务或调用真实 provider。原私有 helper 控制另验证多页、冲突、重复/空页及取消；AppShell 组合 build 和真实页面的旧链接刷新、失败状态及不可发送仍须单独验收。

正式便携入口 `route-formal-check-01` 已在固定基线实际执行，4/4 PASS，退出码 0；结果保存在 `_tmp/20261114-fork-recovery/route-formal-check-01/results.json`（SHA256 `efaa5c734a7d647b2cae716f162ea7a98f9437a45367bcd14998fa98fcae65cd`）。503 负控在 stderr 输出预期错误。此次没有构建网页或启动服务。

Web11 在已有会话切换到新会话时出现 `Cannot read properties of null (reading 'message')`，页面首败由本轮实页记录保留。Web12 仅为错误状态返回表达式增加 null guard。真实 hook 合成负控以 Web11 原 patch 执行同八项：旧四项通过、新四项均以该 TypeError 失败，结果为 `_tmp/20261114-fork-recovery/route-null-negative-01/control-run/results.json`（SHA256 `16153d559d830667d70fc2653e488898ae5bd98f49ea1a839fa4140cdb3d0fe7`）；该负控不是页面首败替代品。Web12 正式便携入口 `route-formal-check-02` 实际 8/8 PASS、exit 0，结果 SHA256 `2e515eb95fc6fa427ceaae6ccdc6816f8f94a11ce2ef6bc8b6c7d9e6832575cc`。新页面组合构建与真实 UI 复验待 root 执行。

更新便携入口说明注释后的最终文件集另行运行 `route-formal-check-03`，8/8 PASS、exit 0，结果 SHA256 `05be13fc2e3805948cb24860e953dca079957a3e013a4cbfee392b174d455126`；`route-formal-check-02` 原件保留。
