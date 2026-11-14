# 占位与空壳盘点（交付标准第 5 条）

- 对着的提交：`4655aed5e`（`docs: 台账与登记表跟上尺子修复的结果`）
- 基线：`OpenBMB/PilotDeck@cc7c4e6`，源码在 `resources/PilotDeck/`
- 范围口径：`specs/delivery/README.md`「范围」一节。范围内：`pilotdeck server`（gateway）、CLI 子命令、独立 sidecar；范围外：阶段 B、前端、StaffDeck、渠道生产实现（只要求启动表现一致）、Rust 原生扩展接口、按页渲染 PDF、写后语法诊断。
- 做法：只读两侧源码，没有构建、没有实测（原因见附录 H）。每条基线结论带文件与行号；Rust 侧的注释只当线索，与代码不符的注释在附录 G 列出。
- 机械判据：`scripts/bbx/status.mjs` 第 5 条已按本次结论补上四类（`Failure::Todo`、自报未实现文案、宿主边界哨兵、原生档案缺口），当前提交下共 102 行命中；`CAPABILITY_UNAVAILABLE` 与「返回固定空结果」没有加，原因见第 4 节。

## 1. 搜索式与命中数

文件范围：`src/*/src/**/*.rs`，排除 `tests/`、`target/`、`fixtures/`、`benches/`、`examples/`、`vendor/` 与 `tests.rs`、`*_tests.rs`、`*_test.rs`、`test_*.rs`（`rg -g` 实现，脚本在 scratchpad，可复跑）。数的是行；没有排除文件内的 `#[cfg(test)]` 块，所以有几行是测试断言，逐条定性时已剔除。

| 搜索式 | 命中行 | 去向 |
|---|---|---|
| `Failure::Todo`（`-F`） | 18 | 15 处构造（附录 A）＋枚举匹配 3 行 |
| `unimplemented!` / `todo!(` / `NOT_IMPLEMENTED` | 1 / 0 / 1 | 一行注释、一行 HTTP 501 状态码，都不是占位 |
| `requires_host` | 118 | 宿主边界（附录 B）；其中 `requires_host\s*(:\|=)\s*true` 13 行 |
| `"(NODE_IO_REQUIRED\|JS_HOST_REQUIRED\|TS_IO_REQUIRED)"` | 24 | 宿主边界哨兵（附录 B） |
| `Route::Forward` | 19 | 元数据方法退回「未配置」（附录 B） |
| `CAPABILITY_UNAVAILABLE\|capability_unavailable` | 69 | 39 行非注释；逐处见附录 C |
| `-i not implemented` / `not supported` / `not yet` | 13 / 24 / 28 | 读过每一行，归入附录 D、E、G 或判为基线原文 |
| `-i \bunsupported` | 317 | 绝大多数是 `pilotdeck-yaml`、MCP 输出 schema 的 `Unsupported` 变体与 sidecar 协议错误；非 yaml/schema 的 60 余行逐行看过 |
| `-i \bstub` / `placeholder` | 22 / 48 | 多数是 UI 字段名 `placeholder`、测试 stub；有意义的进了附录 F、G |
| `-wTODO`（区分大小写） | 12 | CLI 的 `Failure::Todo` 打印与测试、`always_on/plan.rs` 的模糊 TODO 规则（基线行为）、一行 `TODO(#772)` 注释 |
| `FIXME\|\bXXX\b` | 0 | — |
| `待实现\|暂不支持\|未实现\|尚未\|未迁移` | 3 | 「未迁移到 Rust」2 行（渠道拒绝启动）、1 行基线原文 |
| `-i no-op\|not wired\|unwired\|not ported\|no production caller` | 137 | 逐行看过；真缺口是热重载（G1）与 `Families::unwired()`，其余是过期注释（附录 G）或基线同样的 no-op |
| `allow(dead_code)` | 54 | 已写好未接线的线索；`assembly/automation.rs` 的 7 处就是 G1 |
| `\bprofile_gap\(\|(?<![\w.])gap\(` | 42 | 循环与工厂的原生档案缺口（附录 E） |
| `"[A-Z0-9_]*_PROFILE(_UNSUPPORTED)?\b` | 6 | 同上 |
| `-i native gateway\|isolated (native )?process\|this embedding\|this port` | 91 | 读过；有意义的进了附录 C、D |

## 2. 结论

- 范围内的缺口：**11 处**（按功能点合并；对应代码位置 100 余处）
- 范围待定：**7 处**
- 会静默返回空结果或假成功：**2 处**（G1 配置热重载、P5 遥测上报）。其余缺口都是明确报错，文案多数是 Rust 自造。

## 3. 汇总表

### 3.1 范围内的缺口（按「撞上的可能性 × 影响」排）

| # | 缺口 | 用户什么操作会撞上 | Rust 现在给什么 | 基线 | 已写好未接线 | 对拍碰到 | 工作量 | 子能力 |
|---|---|---|---|---|---|---|---|---|
| G1 | **配置热重载整块没有**：服务运行中配置变化后，基线重启 always-on / cron 子系统、重装代理、热启动渠道、切换遥测开关；Rust 的 config store 在 server 里没有对应订阅者 | 服务运行中改 `pilotdeck.yaml`，或界面改设置（`update_settings` 写文件后 store 重载）里动了 `cron` / `alwaysOn` / `proxy` / `adapters` / `telemetry` | **静默**：不生效、不打日志，要重启才生效（`native_process/assembly.rs:1998` `update_subsystems: Arc::new(\|_\| ())`；`config/store.rs:459` `subscribe` 在 server 里只有 refresh 与 settings 两个订阅者） | `src/cli/pilotdeck.ts:132-206` 订阅 `configStore`，打印 `[pilotdeck] Subsystem hot-reload complete: …` / `Adapter hot-reload complete: …`；`ProjectAutomationBundle.ts`（417 行）`reload`、`ChannelAdapterBundle.ts:96` `reload` | 有：`native_process/assembly/automation.rs` 的 `reload` / `stage` / `ProjectAutomationReloadInput`（标着 `#[cfg_attr(not(test), allow(dead_code))]`，只有测试驱动）；`native_process/channels.rs` `hot_start_channel` | 没有（场景里没有 `hot-reload` 字样） | 中 | `config.hot-reload`（另涉 `auto.cron-config`、`auto.always-on-cycle`、`channels.startup`） |
| G2 | **`sdkSessionConfig` 有三种合法写法被拒**：`plugins`（本地插件目录）、基线忽略的未知顶层键、`skills` 列表与 `agents` 同时给 | SDK 客户端开会话时带这些字段 | 整轮 `error` 事件 `CAPABILITY_UNAVAILABLE` `sdk_session_config is unavailable.`（`native_process/sdk_config.rs:1365-1391`，`WIRED_FIELDS` 不含 `plugins`） | `InProcessGateway.ts:3210` 起校验不查未知键；`:3454-3460` 校验 `plugins`；`ProjectRuntimeRegistry.ts:993`、`:1807-1850` `resolveSdkSessionPlugins` 加载插件 | 插件加载器在（`runtime/plugins/loader`），SDK 会话这条没接 | `sdk-config-validate` 只测了 `plugins` 的三种非法值，合法值没测 | 中 | `config.sdk-session`（另涉 `ext.plugins`、`agents.sdk-defs`） |
| G3 | **`request_user_form` 不支持 Draft 2020-12 表单 schema** | 模型给表单 schema 写 `"$schema": "https://json-schema.org/draft/2020-12/schema"`（模型写 schema 时常带这一行） | 工具错误 `schema.$schema https://json-schema.org/draft/2020-12/schema is not supported by this gateway.`（`runtime/tools/builtin/form_dialog.rs:160-164`） | `tool/dialog/FormDialogSchema.ts:136-137`、`:153-165` 用 Ajv 2020 ＋ ajv-formats 编译并校验 | 部分：MCP 输出 schema 有一份 Ajv draft-07 的移植（`runtime/mcp/output_schema.rs`，4538 行），不是 2020-12 | 没有 | 中到大 | `tools.interaction` |
| G4 | **外部模块绑定**：`modules.skills` / `modelProvider` / `context` 写了 `implementationId`（外部 HTTP 模块），或配了 `modules.sop` | 配了外部模块的部署 | `pilotdeck server` 拒绝启动 `Native isolated profile does not yet support configured module bindings.`；带附件的轮次同文案 `CAPABILITY_UNAVAILABLE`（`native_process/assembly.rs:2340-2366`、`services.rs:1397-1399`） | `composition/runtimePorts.ts`（143 行）＋`HttpModuleClient.ts`（180 行），`createLocalGateway.ts:857`、`ProjectSessionRuntimeBundle.ts:252,319` | knowledge / tools / agentLoop 的外部绑定已实现，其余三个槽没有 | `modules.binding` 下有场景，未见这三槽的外部绑定 | 中 | `modules.binding`（`sop` 槽归 StaffDeck，范围外） |
| G5 | **`gateway setup` 里只写本地文件的三步**：微信已有凭据时「重新登录？N」（启用 weixin）、企业微信已有配置时「重新配置？N」、企业微信手动填 Bot ID/Secret 走完问答 | `pilotdeck gateway setup weixin` / `wecom` | `pilotdeck-rs TODO: gateway setup: … is not implemented yet.`，退出码 2（`gateway_setup.rs:269,307,361`） | `gatewaySetup.ts:285-299`、`:369-378`、`:509-515`：改 `adapters.*` 后 `stringifyYaml(config,{lineWidth:0,singleQuote:false})` 整份写回（`:621-631`） | 没有：`pilotdeck-yaml` 只有「在原文档上改」的 stringify（`stringify.rs`），没有「把 JS 值整份序列化」 | 没有（问答部分有场景） | 中 | `cli.setup-update` |
| G6 | **会话记录里少见的条目形状**：消息 `content` 不是数组、`text` 块没有字符串 `text`、`lastModified` 为 NaN 等 | 列会话 / 读会话时碰到手改或损坏的 transcript | 错误 `Session projection of this <kind> entry shape is not implemented (A7.5).` / `Session listing with a NaN lastModified is not implemented (A7.5).`（`storage/transcript_adapter.rs:488-491,1525-1566`） | 基线接受这些形状（Rust 注释自述：只拒绝「TS 接受但没移植」的形状，`transcript_adapter.rs:18`） | 没有 | 没有 | 小到中 | `session.read` |
| G7 | **MCP 工具 `outputSchema` 里 JS 专有正则**（后行断言、反向引用、Rust 表外的 `\p{…}`）、`format: "regex"` 判不了的值、嵌套超过上限 | 接了用这种 schema 的 MCP 服务并调用其工具 | 校验报 `pattern "…" uses JavaScript regular-expression syntax this port does not implement` 等（`runtime/mcp/output_schema.rs:450,2131,2914`） | SDK 的 Ajv 照常校验 | 没有 JS 正则引擎 | 有 output schema 场景，不含这些写法 | 中 | `ext.mcp` |
| G8 | **`update_settings` 遇到少见 YAML 特性**：`%YAML` / 多个 `%TAG` 指令、yaml-1.1 的 set/omap/pairs/binary/timestamp/合并键、孤立代理项 | 用户的 `pilotdeck.yaml` 里有这些，再从界面改设置 | `Unable to write local settings: <特性> is not supported by this host.`（`runtime/settings/local_settings.rs:336-345`，来源 `pilotdeck-yaml/src/stringify.rs:351-360`） | `yaml@2.9.0` `document.toString()` 照常写 | 没有 | 没有 | 中 | `config.settings-api` |
| G9 | **循环与工厂的「原生档案缺口」**：42 行，模型或宿主回来的值形状异常（非数组 content、非字符串文本、非字符串路由目标、未知恢复动作、深度超 1024、顶层 `media_reference` 等） | 正常使用基本碰不到；接入行为怪的 provider 或外部 host 时可能 | 整轮失败，`Rust normal-turn candidate has not implemented <profile>.` / `Rust factory candidate has not implemented …` / `*_PROFILE` 错误码（附录 E） | 基线按 JS 语义照常处理（强制转换、递归等） | — | 没有 | 每条小，合计中 | `loop.recovery`、`loop.tool-dispatch`、`context.compaction`、`modules.sidecar-stdio` |
| G10 | **配置里出现 JS 内建属性名**（`constructor`、`toString`、`__proto__` 等 12 个）作 provider / model 键或选择串，或两份配置都成环且带 `__proto__` | 几乎不会 | CLI 所有读配置的命令（含 `server`）：`pilotdeck-rs TODO: config requiring JavaScript host support is not implemented yet.`，退出码 2（`pilotdeck-cli/src/config.rs:24-28`；判定在 `pilotdeck-gateway/src/config/host.rs:17-77`、`graph.rs:338-347`） | 基线按普通键处理（JS 对象自有属性），未实测 | — | 没有 | 小到中 | `config.load` |
| G11 | **Rust 自造的拒绝**：`submit_turn` 带 `sdkConfig` 字段；`uploadedAttachments[]` 的 `uploadId` / `attachmentIds` 不是字符串；method 名含孤立代理项 | 现有客户端都不发；手写请求才会 | ① `CAPABILITY_UNAVAILABLE` `Per-turn SDK overrides are not installed in this isolated process.`（`services.rs:1157-1162`）② `CAPABILITY_UNAVAILABLE` `uploadedAttachments[].uploadId must be a string.` 等（`services.rs:1264-1303`）③ `Unknown native gateway method.`（`requests.rs:231-233`） | ① 基线没有 `sdkConfig` 这个字段，直接忽略 ② 原样交给 `acquireAttachmentLease`（`GatewayUploadedAttachmentBundle.ts:34-42`），报 provider 自己的错 ③ `Unknown gateway method <name>.` | — | 没有 | 小 | `loop.submit`、`session.uploads`、`gw.ws-protocol` |

### 3.2 范围待定

| # | 是什么 | 为什么说不清 | Rust 现在给什么 | 基线规模 | 用户什么时候撞上 | 建议 |
|---|---|---|---|---|---|---|
| P1 | **渠道配齐凭据时 server 拒绝启动**：基线在这些配置下会真的连外部服务的渠道——`qq`、`wecom`（企业微信 AI Bot）、`mattermost`、`signal`、`whatsapp`、`homeassistant`。基线自己就缺 SDK 的 `telegram` `discord` `slack` `matrix` `dingtalk` `email` 只打一行不可用，Rust 照做，不在此列 | 渠道生产实现不做，但「启动表现一致」在范围内；基线带凭据时起渠道（连外部服务），Rust 则整个 server 不起 | 启动失败、非零退出：`[adapters] channel "<key>" 未迁移到 Rust，需继续使用 TS gateway`（`channel/loader.rs:111-148`、`native_process/assembly.rs:1551-1600`；判定「基线会连」的条件在 `loader.rs:219-` `unavailable_start` 与 `:369-` `bundle_unavailable_start`）。文案让用户回去用 TS gateway，而第 6 条要求 TS gateway 退役 | `adapters/channel/loadEnabledChannels.ts`、`cli/ChannelAdapterBundle.ts:146-183` 与各渠道实现 | 配好 QQ 机器人或企业微信 AI Bot 等的用户升级后起不来 | 影响面最大的一条。要定：带凭据时是「照常启动、该渠道报一行不可用」还是「拒绝启动」，以及文案 **2026-11-14 已解决**：照常启动、打一行不可用、状态记 failed（`state/takeover.md`「人已定」第 12 条）；本行是盘点当时的记录 |
| P2 | `pilotdeck tui`（交互终端里） | 是 CLI 子命令，但本身是 ink/React 终端界面 | `pilotdeck-rs TODO: pilotdeck tui is not implemented yet.`，退出码 2（`main.rs:177-184`）；非终端时与基线一致 | `adapters/channel/tui/` 21 个文件 2519 行，`pilotdeck.ts:318-345` | 在终端里敲 `pilotdeck tui` | 建议归范围外（同前端），或按「渠道只对齐启动表现」处理 |
| P3 | `gateway setup` 的联网三步：飞书凭据校验并写入、微信 iLink 扫码登录、企业微信扫码取 Bot | CLI 子命令，但做的是渠道的登录与凭据校验，会连外部服务 | `TODO` 退出码 2（`gateway_setup.rs:259,281,314`）；飞书一步在打印「🔍 验证凭据...」之前就停了 | `gatewaySetup.ts:152-266`、`:380-457`、`:530-583` | 用 setup 向导配飞书 / 微信 / 企业微信 | 飞书凭据校验失败后基线问「是否仍然保存」，那一支可以离线走到写文件；建议并入 G5 一起做本地写，联网部分归范围外 |
| P4 | `pilotdeck update` 找到 Git Web 更新脚本时 | 属于安装布局：Rust 发行物里没有 `scripts/update-web.mjs`，基线用 Node 去跑它 | 找到脚本：`TODO` 退出码 2（`main.rs:197-209`）；找不到：与基线同文案退出 1 | `runtime/updateCommand.ts` 27 行 ＋ `scripts/update-web.mjs` | 实际 Rust 安装里找不到脚本，走的是一致的那支 | 交付标准已把 `cli-update-usage` 列为「对齐或经人同意」；建议随那条一起定 |
| P5 | **遥测上报**（`ANALYTICS_ENABLED=1`） | 上报到外部收集端，但地址可用 `ANALYTICS_BASE_URL` 指到本机假服务，能测 | **静默**：不入队、不上报，退出时计数不动（`pilotdeck-cli/src/telemetry.rs:6-11,112-118`） | `src/telemetry/` 6 个文件 1269 行 | 打开了遥测的部署 | 默认关闭（`collector.ts:139`），影响小；建议定为范围外并登记 |
| P6 | 公共 host `file_parse` 的 PDF 文本抽取 | 范围外只写了「按页渲染 PDF」；这里是抽文本，但同样依赖 mupdf | 415 `PUBLIC_FILE_TYPE_UNSUPPORTED`（`module_host/runtime.rs:617-624`） | `composition/activeRuntimeTextParsing.ts` | StaffDeck 等通过公共 host 解析 PDF | 建议与「按页渲染 PDF」一起归范围外 |
| P7 | sidecar 的 `PILOTDECK_AGENT_LOOP_FACTORY`（加载用户 JS 模块作循环工厂） | sidecar 在范围内，但这是加载并执行用户提供的 JS 代码，接近「扩展接口」 | 退出码 2 `Rust candidate does not yet support the PILOTDECK_AGENT_LOOP_FACTORY deployment profile.`（`pilotdeck-agent-loop-sidecar.rs:31-36`） | `cli/pilotdeck-agent-loop-sidecar.ts:11-24` | 自定义循环工厂的部署 | 建议归范围外（Rust 不可能执行 JS 模块） |

### 3.3 其余

| 归类 | 计数 | 一行说明（明细见附录） |
|---|---|---|
| 一致 | 约 30 处 | `CAPABILITY_UNAVAILABLE` 的「没配置就拒绝」闸门，Rust 文案与基线逐字相同且 `pilotdeck server` 两侧接线一致（上传、快照、cron、运行登记、manager、记忆、用户对话框、设置、可信上下文、任务预算）；公共 host 的 501 / 409；插件的编程式贡献为空；插件市场与分叉附件两句「不支持」是基线原文；sidecar 宿主 context 的四个可选操作 |
| 范围外 | 6 处 | StaffDeck 项目绑定（CLI `config.rs:19`、公共 host 按主体绑定）、SOP 审批与权威（`methods/sop.rs`、`module_host/approvals.rs`、`read_sop_authority`）、`read_file` 的 PDF 页数与按页渲染（mupdf，已在允许清单 `gap-tool-read-file-vision`） |
| 环境性 | 约 45 行 | 没有 `$HOME`、cwd 被删、拿不到可执行文件路径、内置排序数据加载失败；各方法「没测量过的 errno」（宿主边界哨兵大部分）；本地时间落在夏令时缝隙 / 闰秒时区 |
| 不可达防御 | 约 12 处 | Rust 内部「资源没装上」的兜底（`Native project resources are missing.`、`Workspace snapshot cwd is unavailable.` 等），生产接线下走不到；文案是 Rust 自造，若真走到属于 bug |

## 4. 判据改动

`scripts/bbx/status.mjs` 第 5 条在原来三项之外加了四项，每项列出位置：

| 判据 | 正则（摘要） | 当前命中 |
|---|---|---|
| `Failure::Todo` | `\bFailure::Todo\((?!\s*\w+\s*\))`，匹配分支 `Err(Failure::Todo(what))` 不算 | 15 |
| 自报未实现文案 | `candidate has not implemented`、`does not yet support`、`is not implemented (A\d`、`not available in the native runtime yet`、`this port (does not\|supports\|cannot)`、`by this port`、`is not supported by this (gateway\|host)`、`未迁移到 Rust`、`isolated (native )process.`、`Native gateway (method\|cannot)` | 18 |
| 宿主边界哨兵 | `"(NODE_IO_REQUIRED\|JS_HOST_REQUIRED\|TS_IO_REQUIRED)"`、`requires_host\s*(:\|=)\s*true` | 27 |
| 原生档案缺口 | `"…_PROFILE(_UNSUPPORTED)?\b`、`profile_gap(`（定义除外）、`gap("` | 42 |

另外跳过顶层 `#[cfg(test)] mod x { … }` 块（以行首 `}` 结束），不再把测试断言算进去。`status-selftest.mjs` 加了 5 项检查（构造与匹配分支、跨行构造、注释、测试块、`false` 与 `*_PROFILE_INVALID` 不算），故意去掉 `Failure::Todo` 的排除条件后有 2 项报 FAIL，还原后全过。

没加的：`CAPABILITY_UNAVAILABLE`（基线大量使用，要逐处读）；返回固定空结果（`Vec::new()` 之类写法太多，误报远多于命中；本次逐个看过的 `collect_*_contributions` 与基线一致）；`allow(dead_code)`（是「写好未接线」的好线索，但大部分是协议类型的未读字段，作判据噪声太大）；自报文案只按已知措辞列举，以后的新措辞要补进 `PLACEHOLDERS`。

---

## 附录 A：`Failure::Todo` 15 处

CLI 统一输出到 stderr：`pilotdeck-rs TODO: <说明> is not implemented yet.`，退出码 2（`pilotdeck-cli/src/main.rs:151-156`）。

| 位置 | 触发 | 基线 | 归类 |
|---|---|---|---|
| `main.rs:141` | 当前目录不可读（被删） | `process.cwd()` 抛 ENOENT，`main().catch` 打印消息、退出 1（`pilotdeck.ts:37,625-628`） | 环境性 |
| `main.rs:183` | `pilotdeck tui` 且 stdin 是终端 | 起 TuiChannel（`pilotdeck.ts:318-345`） | 待定 P2 |
| `main.rs:202` | `pilotdeck update` 且某级父目录有 `scripts/update-web.mjs` | 用 Node 跑它（`updateCommand.ts:14-27`） | 待定 P4 |
| `gateway_setup.rs:259` | 飞书填完 App ID / Secret | 校验凭据（联网）后写 yaml（`gatewaySetup.ts:152-171`） | 待定 P3 |
| `gateway_setup.rs:269` | 微信已有凭据，「重新登录？」答 N | `enableWeixinConfig()` 写 yaml 并打印「✅ 微信已启用（使用现有凭据）」（`:509-515,604-609`） | 范围内 G5 |
| `gateway_setup.rs:281` | 微信继续扫码登录 | `weixin-ilink` 扫码（联网，`:530-583`） | 待定 P3 |
| `gateway_setup.rs:307` | 企业微信已有配置，「重新配置？」答 N | `writeWeComConfig(现有值)`、打印「企业微信已启用（使用现有凭据）。」（`:285-299`） | 范围内 G5 |
| `gateway_setup.rs:314` | 企业微信选扫码 | 请求企业微信二维码接口（联网，`:380-457`） | 待定 P3 |
| `gateway_setup.rs:361` | 企业微信手动配置问答走完 | `writeWeComConfig` 写 yaml（`:369-378`） | 范围内 G5 |
| `paths.rs:65` | 没有 `HOME` 且 `PILOT_HOME` 以 `~` 开头（或未设） | `os.homedir()` 回落到 passwd 记录，照常运行 | 环境性 |
| `skills_migrate.rs:123` | `skills migrate` 时没有 `HOME` | 同上 | 环境性 |
| `skills.rs:29` | 没设 `PILOTDECK_BUNDLED_SKILLS_DIR` 且拿不到可执行文件路径 | 基线用 `import.meta.url`，不会缺 | 环境性 |
| `skills.rs:256` | 内置 ICU 排序数据建不出 collator（数据编译进二进制，实际不会） | Intl 总可用 | 环境性 |
| `config.rs:19` | 环境变量 `STAFFDECK_PROJECT_BINDING_ENABLED=true`，任何读配置的命令 | `applyProjectBinding`（StaffDeck 项目绑定） | 范围外（StaffDeck）。注意：gateway 侧 `config/binding.rs` 有只读实现，CLI 没用 |
| `config.rs:26` | 配置加载返回 `requires_host`（附录 B 第 1 组） | 见 G10 | 范围内 G10 / 环境性 |

## 附录 B：宿主边界（`requires_host` 与哨兵）

上一轮的设计：Rust 遇到「没对着基线测量过」的情况不猜，标 `requires_host` 交给 TS 宿主。TS 退役后这些分支落成下面三种输出：

- CLI 读配置：`Failure::Todo("config requiring JavaScript host support")`，退出码 2。
- 管理类方法（skill_*、snapshot_*、memory_*）：`CAPABILITY_UNAVAILABLE` `Native gateway cannot execute this host-dependent operation.`（`native_process/management.rs:30-35,105-106,241-243,268-270`）；`background_tasks` / `background_task_stop`：`CAPABILITY_UNAVAILABLE` 加原消息（`methods/tools.rs:72-86`）。
- 元数据方法（`list_projects`、`describe_project`、`project_files_list`、`project_file_read`、`model_catalog_list`、`session_model_get`、`native_archive_*`）：`dispatch.rs` 返回 `Route::Forward`，再变成 `CAPABILITY_UNAVAILABLE` `Native gateway method <method> is not configured.`（`methods/metadata.rs:61-73`）。`Route::Forward` 的来源：`dispatch.rs:102,118,123,135,139,168,174,183,190,305,309,310`；`project_files.rs:784`（孤立代理项的 projectKey）、`:823,827`（Node 不会给出的参数描述）、`:836`（未测量 errno）；`model_catalog.rs:232`、`session_model.rs:74`（`requires_host`）、`session_model.rs:283`；`native_archive.rs:573-574`（`TS_IO_REQUIRED`）。

| 组 | 位置 | 触发 | 基线 | 归类 |
|---|---|---|---|---|
| 1 配置里的 JS 内建属性 | `config/mod.rs:347-354`（判定 `config/host.rs:17-77`，名单 `config/value.rs:134-147`）、`config/graph.rs:338-347` | 见 G10 | 按普通键处理（未实测） | 范围内 G10 |
| 2 配置读取的未测量 errno | `config/mod.rs:509-535`、`config/binding.rs:27-28,105,137` | 读 `pilotdeck.yaml` / 绑定文件时遇到 ENOENT、EACCES、ENOTDIR、EISDIR、ENAMETOOLONG、ELOOP 之外的 errno（EIO、EMFILE 等） | Node 报 `<CODE>: <描述>, open '<path>'` | 环境性。Rust 已有完整 libuv 文案表（`node_io/generated.rs`），放开准入是小改动 |
| 3 项目文件 | `project_files.rs:78-84,955-966,833-840` | 列 / 读项目文件时 ENOENT、EACCES、ENOTDIR、ENAMETOOLONG、ELOOP 之外的 errno | Node 原文错误 | 环境性 |
| 4 会话模型与目录 | `session_model.rs:330-350`、`model_catalog.rs:232` | 参数描述 Node 会抛别的错（如 projectKey 是带怪 `constructor` 的对象）；未测量 errno | Node 原文错误 | 环境性（参数形状那支算手写请求） |
| 5 归档 | `native_archive.rs:285,293,574` | 未测量 errno（准入六种） | Node 原文错误 | 环境性 |
| 6 skills | `skills/error.rs:161-176`（`host()` 在 `:79,187` 用） | 各方法测量过的 errno 很窄：`skill_read` 只有 ENOTDIR、EISDIR，`skill_list/create/delete/scan` 只有 ENOTDIR（`node_io.rs:40-43`）；技能文件 EACCES 就会落到这里 | Node 原文错误（如 `EACCES: permission denied, open …`） | 环境性；但权限问题在真实机器上不罕见，改起来小（同第 2 组） |
| 7 快照 | `runtime/snapshot.rs:102-108,198,646,658` | `snapshot_list` / `snapshot_get` 任何 fs errno、`snapshot_restore` 除 ENOENT 外的 errno | Node 原文错误 | 环境性 |
| 8 记忆 | `runtime/memory/mod.rs:115-122,819`、`edgeclaw/factory.rs:213`、`edgeclaw/service/mod.rs:192`、`edgeclaw/storage.rs:1432` | 默认项目根不是合法 UTF-8；会话键含孤立代理项；排序数据建不出 | 照常 | 环境性 |
| 9 后台任务 | `runtime/background.rs`（`BackgroundProjectionError.requires_host` 透传项目工厂的错误） | 项目运行时工厂失败且原错误带 `requires_host` | 原错误 | 环境性（跟着前几组） |

## 附录 C：`CAPABILITY_UNAVAILABLE` 逐处

| 位置 | 文案 | 触发 | 基线同文案 | 归类 |
|---|---|---|---|---|
| `upload/gateway.rs:123-127` | `Gateway uploads are unavailable.` | 没接上传 | `InProcessGateway.ts`（`requireUploadLifecycle`），server 两侧都接 | 一致 |
| `runtime/snapshot.rs:369-374` | `Gateway snapshot provider is unavailable.` | 没有快照存储根 | `createLocalGateway.ts` 3 处同文案 | 一致 |
| `runtime/cron/projection.rs:30-37` | `Cron runtime is not configured on this gateway.` | 没接 cron | `InProcessGateway.ts:3016-3022` | 一致 |
| `runtime/turn/admission.rs:596-610` | 可信上下文 | 非空 `trustedContext` 且没有授权器 | `pilotdeck server` 不传 `trustedContextAuthorizer`（`createLocalGateway.ts:207,1049`） | 一致 |
| `runtime/turn/admission.rs:760-768` | `Gateway task-budget accounting is unavailable.` | `taskBudget` 但没有记账 | `InProcessGateway.ts:1171-1181` | 一致 |
| `runtime/turn/admission.rs:872-878`、`submit.rs:441-447` | `sdk_session_config is unavailable.` | 没有 SDK 配置端口 | `InProcessGateway.ts:1021,2432` | 一致（端口存在时的拒绝见 G2） |
| `native_process/sdk_config.rs:1365-1391` | `sdk_session_config is unavailable.` | 端口在，但字段未接 | 基线处理这些字段 | **范围内 G2** |
| `runtime/manager/projection.rs:168-174,232-240` | manager 会话 / 浏览器 provider | 没注入 | `InProcessGateway.ts` 同文案；server 两侧都不传 `managerBrowsers` | 一致 |
| `runtime/turn/read.rs:347-351` | `Persistent run registry is unavailable.` | 没有运行登记 | `InProcessGateway.ts:2525-2532` | 一致 |
| `runtime/memory/gateway.rs:146-152`、`memory/mod.rs:833-840,861-868` | `Gateway memory provider is unavailable.` | 没有记忆管理 | `InProcessGateway.ts`、`createLocalGateway.ts` 同文案 | 一致 |
| `methods/user_dialog.rs:34-40,94-104` | 用户对话框 | 总线没连 | `GatewayWsConnection.ts:481` | 一致 |
| `runtime/settings/gateway.rs:448-454` | `<method> is unavailable.` | 设置委托没绑 | `InProcessGateway.ts` 同形；生产全绑 | 一致 |
| `module_host.rs:342`、`:1380-1394` | `PILOTDECK_HOST_CAPABILITY_UNAVAILABLE`（409）、`PUBLIC_HOST_CAPABILITY_UNAVAILABLE`（501） | 公共 host 未广告的操作 | `publicHostHttpTransport.ts:37`、`nativeHostCapabilityProvider.ts:85` | 一致。注：`ModuleHostPorts`（`module_host.rs:1203-1375`，只广告 3 个操作）在生产里没有构造处，生产走的是 `module_host/runtime.rs` 的 `RuntimeHost`（与 `publicHostRuntimeAdapter.ts` 对齐）；前者是死代码 |
| `native_process/management.rs:30-35` 及调用 `:105,106,142,146,228,232,242,248,269` | `Native gateway cannot execute this host-dependent operation.` | `:105,242,269` 是附录 B 的宿主边界；`:106,142,146,228,232,248` 是资源没装上 / 方法名对不上 | — | 环境性 / 不可达防御 |
| `methods/metadata.rs:61,67-73`、`methods/mod.rs:114-118` | `Native gateway method <m> is not configured.` | 元数据方法 `Route::Forward`；或基线方法表里有、Rust 没登记的方法（本次核对 101 个基线方法全部已登记，这支走不到） | — | 环境性（附录 B） |
| `methods/tools.rs:72-86` | 原消息 | 后台任务 `requires_host` | — | 环境性 |
| `methods/sop.rs:13-20,131-134` | `StaffDeck SOP is not configured on this isolated native process.` | `sop_*` 带审批人授权且配了 StaffDeck | 基线去连 StaffDeck | 范围外 |
| `native_process/services.rs:1157-1162` | `Per-turn SDK overrides are not installed in this isolated process.` | 请求带 `sdkConfig` | 基线无此字段 | 范围内 G11 |
| `native_process/services.rs:1264-1303` | `uploadedAttachments must be a non-empty array.` 等 3 句 | 畸形附件引用 | 基线无此文案 | 范围内 G11 |
| `native_process/services.rs:1340-1349,1371-1380` | 线程 join 错误 | 阻塞任务 panic | — | 不可达防御 |
| `native_process/services.rs:1397-1399` | 模块绑定文案 | 同 G4 | — | 范围内 G4 |
| `runtime/turn/submit.rs:549-556,1098-1103`、`submit_host.rs:225-231` | `Workspace snapshot host capability is unavailable.` / `Workspace snapshot cwd is unavailable.` / `Session cwd host capability is unavailable.` | 快照必需但记录器没装 / 会话没有 cwd / cwd 服务没装 | 基线无这三句 | 不可达防御（生产都装了；未实测） |
| `runtime/snapshot.rs:1126`、`memory/gateway.rs:314,322,368`、`methods/config.rs:3295` | — | 测试断言 | — | 不计 |

其余 30 行是注释。

## 附录 D：自报未实现文案 18 处

| 位置 | 文案 | 归类 |
|---|---|---|
| `pilotdeck-agent/src/turn.rs:4064` | `Rust normal-turn candidate has not implemented {profile}.` | G9（30 个调用点见附录 E） |
| `pilotdeck-transport/src/default_factory.rs:1149` | `Rust factory candidate has not implemented {profile}.` | G9 |
| `pilotdeck-transport/src/bin/pilotdeck-agent-loop-sidecar.rs:33` | `… does not yet support the PILOTDECK_AGENT_LOOP_FACTORY …` | 待定 P7 |
| `channel/loader.rs:146,464` | `未迁移到 Rust，需继续使用 TS gateway` | 待定 P1 |
| `native_process/assembly.rs:2363` | `Native isolated profile does not yet support configured module bindings.` | 范围内 G4 |
| `native_process/management.rs:33` | `Native gateway cannot execute this host-dependent operation.` | 附录 B |
| `native_process/methods/metadata.rs:70` | `Native gateway method {method} is not configured.` | 附录 B |
| `native_process/methods/sop.rs:133` | `StaffDeck SOP is not configured on this isolated native process.` | 范围外 |
| `native_process/services.rs:1161` | `Per-turn SDK overrides are not installed in this isolated process.` | 范围内 G11 |
| `runtime/mcp/output_schema.rs:450,2131,2914` | `… this port …` | 范围内 G7 |
| `runtime/project/factory.rs:644` | `Project runtime resource family … is not available in the native runtime yet` | 不可达防御：`Families::unwired()` 只在组装时被各自真实的 family 覆盖后使用（`assembly.rs:2582`、`assembly/mcp.rs:242`、`assembly/memory.rs:334` 都是 `..Families::unwired()` 补齐，未实测每一项都被覆盖） |
| `runtime/settings/local_settings.rs:342` | `… is not supported by this host.` | 范围内 G8 |
| `runtime/tools/builtin/form_dialog.rs:162` | `… is not supported by this gateway.` | 范围内 G3 |
| `storage/transcript_adapter.rs:490,1564` | `… is not implemented (A7.5).` | 范围内 G6 |

## 附录 E：原生档案缺口 42 处（G9）

都是「遇到没测量过的值形状就整轮失败，不猜」。正常的 Rust gateway ＋ Rust sidecar 组合下，host 一侧的回复由 Rust 自己产生，下面多数分支只有模型 provider 回了怪值或外部 host 行为不同时才走到。

- `pilotdeck-agent/src/turn.rs` 的 trait 默认方法 8 处（`:148,154,161,168,176,228,243,254`）：生产实现 `pilotdeck-transport/src/default_factory.rs:231-730` 全部覆盖，走不到。
- `turn.rs` 循环体 22 处：`:1090` 大文件修复查询失败；`:1174,2028` 工具结果投影失败；`:1750` host 没广告 `execute_batch`（单工具调度）；`:1839` 工具结果消费 profile 未知；`:1950` 批量结果与调用数不等；`:2041,2049` 上下文回执是 boundary-only；`:2374,2520` 压缩替换是 boundary-only；`:2824` 恢复用的 assistant content 不是数组；`:3234` 模型错误给了未知恢复动作；`:3259` 路由目标是非字符串真值；`:3406,3467` 数字 / 对象序列化失败；`:4179` 用量字段不是数字；`:4200` 执行路由不是字符串；`:4508,4536` 消息 content 非数组 / text 非字符串；`:4675` 请求指纹序列化为 undefined；`:4830` 输出恢复计划处在错误阶段。
- `pilotdeck-transport/src/default_factory.rs:750,752,773,778,911,913,927`：PlanTodo 观察者回复形状、AgentEvent 不是对象或没有 type、元数据回复不对。
- `pilotdeck-agent/src/request_assembly.rs:96`（缓存序列化深度 ≥1024）、`:955`（顶层 `media_reference` 需要宿主文件物化）；`tool_context_policy.rs:125`（Array.toString 递归深度）。
- `pilotdeck-transport/src/server.rs:449` `UNSUPPORTED_DEADLINE_PROFILE`、`system_clock.rs:371,432`（闰秒时区、夏令时缝隙里的本地时间）：归环境性。

## 附录 F：没有机械标记的缺口与一致项

| 位置 | 是什么 | 归类 |
|---|---|---|
| `native_process/assembly.rs:1945-1998`、`assembly/automation.rs:170-210,375-385,540-555` | 热重载：`update_subsystems` no-op，`reload` 无生产调用 | 范围内 G1（静默） |
| `pilotdeck-cli/src/telemetry.rs` | 不上报遥测 | 待定 P5（静默） |
| `runtime/plugins/contributions.rs:565-577` | 工具 / 路由 / 提示词三类编程式贡献返回空 | 一致：基线磁盘插件与内置插件都是 JSON，没有编程式贡献（`extension/plugins/protocol/plugin.ts:22-25`、`builtin/loadBuiltinPlugins.ts:26-32`） |
| `runtime/project/custom_routers.rs:12-33` | 自定义路由查找恒为 None | 一致（同上） |
| `host/modules/context.rs:53-75` | context 四个可选操作的默认「不支持」 | 一致：基线同文案 `sidecarDefaultModuleDispatcher.ts:375`；生产实现 `runtime/context/prepare.rs:835-930` 五个操作都支持 |
| `runtime/plugins/loader/config.rs:174`、`native_process/session_fork.rs:741` | 「installation is not implemented」「Forking … is not supported yet.」 | 一致：基线原文（`marketplace.ts:16`、`web/server/forkSession.ts:365`） |
| `storage/file_history.rs:7` | 文件历史的 `backupStorage` 未移植 | 一致：只有注入了带 `fileHistoryBackups` 的异步 transcript 存储才有（`AsyncTranscriptStorageAdapter.ts:264-300`），`pilotdeck server` 默认不注入（未实测） |
| `module_host/runtime.rs:617-624` | 公共 host PDF 抽文本 | 待定 P6 |
| `runtime/tools/builtin/read_file.rs:665-750` | PDF 页数、按页渲染 | 范围外（允许清单） |
| `module_host/approvals.rs:1000-1015`、`module_host/runtime.rs:16-23` | StaffDeck 审批认证、SOP 权威、按主体的项目绑定 | 范围外 |

## 附录 G：与代码不符的注释（不是占位，记一笔免得误导）

- `module_host.rs:7,30`、`module_host/approvals.rs:9-13,19`：说「没接进 native_process」，实际已接（`native_process/server.rs:224`、`native_http.rs:125,161`、`assembly.rs:538`）。
- `storage/transcript_adapter.rs:7`、`storage/mod.rs:4`、`skills/mod.rs:4`、`model_cost.rs:12`：说「没接进 gateway」，实际都有生产调用方。
- `runtime/always_on/mod.rs:8-14`、`always_on/fire.rs:6-9`：说 `run` / `rerunPlan` 回 `internal`「not implemented in this embedding」，现在 `fire.rs:459-527` 两者都已实现，代码里没有这句文案。
- `pilotdeck-gateway/src/lib.rs:5`「Unwired channel component」。
- `channel/slack/` 是一份写好的 slack 渠道，但生产挂载表不再用它（`native_process/channels.rs:116-118`：基线不带 `@slack/bolt`，只会打一行不可用），属于有意不接，不是缺口。

## 附录 H：没查清的

- 全程没有构建、没有实测。原因：能用来对照的 CLI 命令（`server`、`sessions`、`cron`、默认对话）要么监听、要么探测 127.0.0.1:18789，在本机跑有碰到真实 PilotDeck 的风险；其余结论读源码已足够。下面几条读源码定不了，建议上 bbx 实测：
  - G10：基线遇到 provider 名为 `constructor` / `toString` 的配置实际做什么。
  - 附录 C「不可达防御」三句与 `Families::unwired()`：是否真的走不到。
  - G9 里 `turn.rs:1750`（host 不广告 `execute_batch`）：独立 sidecar 被别的 host 驱动时能否走到。
  - `file_history` 的 `backupStorage` 在 `pilotdeck server` 下是否确实不注入。
- G6 的「基线接受」只核对了 Rust 侧的判定条件与注释，没有逐个形状去读基线 reducer。
- `CAPABILITY_UNAVAILABLE` 一致项里「server 两侧接线一致」依据的是对拍场景存在且基线 `pilotdeck.ts` / `createLocalGateway.ts` 的接线，没有逐个方法实测未接线时的分支。
