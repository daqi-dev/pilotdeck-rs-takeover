# H1 剩余渠道恢复盘点与 WhatsApp 候选审查

日期：2026-11-13。负责人：h1_inventory（只读审查与拆包）。根起点 `e8618392e7e4eb24c227eedd28e14c84237ca8a4`；C1 HEAD `bcad7cb1435103e31c9ec71877c7064f3cffc619` 加未提交候选。固定参考 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，本轮实际重新读取源码；未运行 Cargo、产品或真实服务，未重新执行 TS oracle。StaffDeck 延期，本报告不将其专属身份/绑定/SOP列为本期实施项。

本报告是任务拆分材料，不替代唯一总进度 `state/backend-handoff.md`。本轮证据是源码和本地已有夹具；C1旧测试结果只按 `state/c1-cli-handoff.md` 引述，不能当作当前候选验证。文件哈希快照：`_tmp/20261113-h1-remaining-resume/inventory.json`。

## 重新核实的差距

根 `channel/loader.rs:46-58` 仍拒绝11渠道；拒绝表本身不能说明实现是否存在。根 `channel/mod.rs`、目录清单和 `native_process/channels.rs` 联合核对结果如下。下表路径以 `src/pilotdeck-gateway/src/` 为前缀。

| 渠道 | 根代码事实 | C1候选事实 | 本期剩余工作 |
|---|---|---|---|
| email | 无 `channel/email/`，没有生产注册 | 同样没有目录 | 完整adapter/IMAP/SMTP及正式入口；不是补一行mount |
| dingtalk | 无 `channel/dingtalk/`，没有生产注册 | 同样没有目录 | 完整stream接入、webhook出站、交互与重连 |
| matrix | 无 `channel/matrix/`，没有生产注册 | 同样没有目录 | 完整sync/invite/send及磁盘sync状态 |
| discord | 无 `channel/discord/`，没有生产注册 | 同样没有目录 | 完整Gateway/REST、身份、事件过滤及互动 |
| telegram | 无 `channel/telegram/`，没有生产注册 | 同样没有目录 | 完整Bot API polling/webhook分支、发送及互动 |
| qq | 无 `channel/qq/`，没有bundle构造 | 同样没有目录 | 完整token/WS/REST及mapper持久化；不能放入普通loader冒充bundle |
| wecom | 无 `channel/wecom/`，没有bundle构造 | 同样没有目录 | 完整aibot WS、媒体、策略、mapper持久化 |
| wecomCallback | 已有 `channel/wecom_callback/{mod,http,crypto,mapper,render,xml,tests}.rs`；`native_process/channels.rs:120-124`已mount | 已保留 | 现有实现正式回归；root mount传 `no_interaction()`，互动接线仍需核实/补齐 |
| whatsapp | 无目录、拒绝 | 9 Rust文件、11夹具文件；未mod、未mount、仍拒绝 | 先审查/早编译，再受控接线与正式验证 |
| homeassistant / signal / mattermost | 根未实现、拒绝 | C1已有目录与mount，shared/network/WS/reload候选 | 按C1 handoff修复首次失败并重验，独立集成；本报告不重复重写 |

所有7个空缺目录均以实际文件清单核对，不采信旧 entries.json 的状态。`tests/fixtures/channel-loader/<name>/*Channel.js`只是构造参数探针，不是产品实现；不能因这些文件存在而声称渠道已迁移。`wecomCallback`与`wecom`分别是HTTP加密回调和WebSocket aibot通道，token/secret、鉴权、状态和发送协议不可互换。

## 可直接独立派工的模块

统一正式入口：隔离 HOME/PILOT_HOME 中启动 native `pilotdeck server`，由真实配置 `adapters.<key>.enabled`触发；本地mock向渠道入站，真实Gateway→本地模型→渠道出站；每包覆盖热更新禁用/重启与cron（基线具有该方法的渠道）。先TS自比和真实错误负控，再Rust。SDK transport函数单测只构成实现层证据。下列新增目录由作者独占；公共 `channel/mod.rs`、`loader.rs`、`native_process/channels.rs`、`assembly.rs`、config和Cargo仅由主协调指定唯一接线人修改。

表中TS路径均以 `resources/PilotDeck/src/`为前缀；均是本次读取的固定源码行号。SDK底层正常网络协议如需重实现，要先用实际SDK+mock抓请求，不根据平台当前文档猜测，避免追新版本。

| 优先包 | 作者独占范围 | 固定基线入口与行为点 | 最小验收矩阵 |
|---|---|---|---|
| H1-Telegram | 新 `channel/telegram/**`，新 `tests/fixtures/h1-telegram/**`，本包交付文档 | `adapters/channel/loadEnabledChannels.ts:12-18`；`telegram/TelegramChannel.ts:40-93` token/env、webhook与poll；`:97-197`互动/忙碌/渲染；`:197-219`发送/typing | 缺token；poll保留pending；webhook配置分支；文本、空值、同chat busy；permission/elicitation先于busy；发送失败；UTF-16分块；cron；取消/重启不重放 |
| H1-Matrix | 新 `channel/matrix/**`，新 `tests/fixtures/h1-matrix/**`，本包文档 | loader`:30-38`传pilotHome；`matrix/MatrixChannel.ts:48-119`storage/getUserId/invite/start/stop；`:123-196`自身、编辑、notice、非text过滤；`:233`sendMessage | homeserver/env/nullish；whoami失败与userId fallback；邀请入房；self/edit/notice过滤；sync cursor隔离落盘/重启；互动/发送失败/cron；热更新取消 |
| H1-Email | 新 `channel/email/**`，新 `tests/fixtures/h1-email/**`，本包文档 | loader`:86-89`只传extra；`email/EmailChannel.ts:56-141`配置/TLS/UID priming/SMTP verify；`:154-214`锁、未读UID、原始正文解码；`:216-324`互动/发送 | TLS端口465与587、配置/env优先；UID前300条priming和重复；mailbox锁失败、verify失败清理；原始Content-Type大小写/quoted printable与8000 UTF-16边界；未知/自身地址；subject和完整mail字段；cron；停机/重启 |
| H1-DingTalk | 新 `channel/dingtalk/**`，新 `tests/fixtures/h1-dingtalk/**`，本包文档 | loader`:68-74`clientId/secret；`dingtalk/DingTalkChannel.ts:46-97`DWClient配置/事件/连接；`:103-217`topic/data/id dedup/webhook/互动；`:254`发送 | client字符串强转；topic过滤；错误JSON；msgId别名/随机fallback；dedup容量；sessionWebhook校验与更新；文本提取；permission首次发送失败重试；HTTP正文失败与cron；stream断线/禁用 |
| H1-Discord | 新 `channel/discord/**`，新 `tests/fixtures/h1-discord/**`，本包文档 | loader`:19-22`token；`discord/DiscordChannel.ts:39-101`Client intents/ready/login/stop；`:107-175`bot/system/self/text过滤；`:213-248`fetch channel/chunks/typing | token/env；握手身份、Gateway事件过滤；DM/group映射；互动优先级；fetch channel失败/非可发送channel；UTF-16分块；typing失败不终止；重连、stop、cron |
| H1-QQ | 新 `channel/qq/**`，新 `tests/fixtures/h1-qq/**`，本包文档 | `cli/ChannelAdapterBundle.ts:154-167`strict true及state；`qq/QQChannel.ts:39-114`凭据/allowGroups/trigger/stop；`:116-420`group/C2C及分块；`qq/qqbot-gateway.ts:65-110,162-311`token/hello/identify/resume/heartbeat | strict enabled原始值；env/default与allowlist/前缀；group与C2C独立msg_id/msg_seq；token刷新/失败；identify/resume序列；heartbeat/reconnect；mapper重启/state写失败；互动/分块；取消 |
| H1-WeCom-WS | 新 `channel/wecom/**`，新 `tests/fixtures/h1-wecom-ws/**`，本包文档 | bundle`:169-180`strict true/state；`wecom/WeComChannel.ts:198-278`配置/启动/停；`:278-426`subscribe/req map/heartbeat/backoff；`:519-978`命令/回调/batching/媒体/turn；`:1042-1313`出站上传；`:1489`deliverable提取 | subscribe成功/超时/错误；回调别名去重、批合并；DM/group策略；普通会话/new及state重启；文本chunk与MEDIA路径；媒体解密/限额/上传分片及失败；请求相关性、ping、重连；停止待请求/活动turn。StaffDeck专属新增能力延期，不擅自删除通用渠道命令 |

建议可并行启动 Telegram、Matrix、Email 三个独立作者，DingTalk/Discord次批；QQ、WeCom-WS各是独立较大包，后者优先早做协议oracle，不能在最后用wecomCallback覆盖。依赖选择先由作者提出；Cargo/公共HTTP和WS接口集中接线，避免每渠道引入一套网络栈。现有C1网络候选与shared取消验收前，新作者可用窄transport seam开发，不等待它们全部交付。

## WhatsApp候选审查

候选位置为C1工作树 `src/pilotdeck-gateway/src/channel/whatsapp/`：`mod.rs`、`bridge.rs`、`lifecycle.rs`、`network.rs`、`wire.rs`、`mapper.rs`、`render.rs`、`tests.rs`、`process_tests.rs`。静态数16个普通测试+4个Unix进程测试，合20；未编译未执行，不能称通过。原正式捕获装置 `tests/fixtures/h1-whatsapp/{capture,bridge,compare}.mjs`提供5场景；value/interaction oracle不能替代正式入口。旧证据路径与失败边界见C1 handoff和该fixture README，本轮没有重新运行。

依赖审查：`mod.rs`使用C1 `im_interaction::{JsInteractionPort,ResponderRef,js_helper_interaction}`和关闭清理能力，使用 `TurnEventStream::cancel_and_drain`，`register_with_network`依赖C1 `channel::network::ChannelNetwork`，HTTP复用native fetch/ModelHttpTransport；`bridge.rs`依赖rustix process/fs、child signal reset、Unix process_group和waitid NOWAIT。不是9文件拷入根再加pub mod就可独立编译的包。默认核心不运行Node；只有启用WhatsApp后按PATH启动用户自带bridge，属于可选外部工具。必须给发行说明交代所需Node和脚本；不得捆回旧TS Gateway/Agent。

已知差异要逐项追加窄账本并用正向断言验收，不修改冻结总表：

1. TS `WhatsAppChannel.ts:72-84`pipe stdout却只读stderr；候选 `bridge.rs`并行排stdout/stderr。保留真实4MiB stdout后listen的基线失败证据，Rust必须ready成功且释放进程。
2. TS `WhatsAppChannel.ts:316`起cleanup只管理直接child；候选Unix创建独立组、leader waitid NOWAIT、最后一次TERM/KILL后reap。必须在Linux/macOS用记录的父孙PID确认关闭，覆盖TERM忽略、leader先退出、start等待者drop；不按名称杀进程。Windows未验收且不具同等组保证。
3. TS `WhatsAppChannel.ts:112-123`停止仅poll abort/child清理；候选停止精确accepted turn并drain。需明确cancel失败可重试、外部stop future drop不遗失owner，正式热禁用模型请求关闭和落盘一致，不能靠新session取消冒充。

本轮静态发现的阻止直接放行项：

- **普通start错误不一致**：TS `WhatsAppChannel.ts:70-108`包围整个启动流程，catch记录 `whatsapp: start failed: ...`、cleanup后返回noop；候选 `mod.rs`的 `bridge::spawn(...)?`把spawn/pipe setup错误直接向外传播。spawn Node失败、脚本路径不存在（Node自身退出）、readiness失败是三条不同路径，需实际TS取证并补精确Rust行为；不能把此差异并入前述三项bug批准。
- **Windows阻塞reader风险**：`bridge.rs`只有Unix将pipe设为NONBLOCK，其他平台drain在阻塞read中不能响应done；继承pipe的孙进程可能使scope join不返回。当前Linux/macOS目标可单独验证；跨平台发行不能据Unix成绩过门。
- stderr逐64KiB块独立UTF-8有损解码，TS按Node data块解码；跨块UTF-8/UTF-16和日志trim边界未证明完全一致。先做真实stderr分块oracle，再决定是否需要重用流式decoder。此为待取证风险，非已确认普通行为bug。
- 组所有权设计依赖waitid NOWAIT的平台可用性/权限及rustix features；首次挂载必须真实all-targets check，不能仅格式检查。

最小接线由唯一公共接线人实施：先在C1已冻结shared/network接口上单独加入 `pub mod whatsapp`；保留拒绝、先编译/跑20项；修上述普通错误及新增窄测试后，在普通与network两种mount构造路径注册 `whatsapp::register[_with_network]`，注意network最后使用处clone；仅移除拒绝表的whatsapp，保留loader既有bridgePath/bridgeUrl转换；同步loader/startup负控的明确集合；连接真实NativeInteractionResponder和ConfigStore，先验证已有渠道无回归。不要手动改写15键loader顺序，不把qq/wecom塞进loader。

正式验收：固定同一源码SHA和新产物目录，Node22.22.0重新TS自比五场景，compare self-test与真实错误回复负控，native同驱动五场景，三项bug正向断言，gateway/CLI受影响clippy/测试与已迁移渠道回归。20单测全部通过也只达到局部验证；真实bridge进程清理、热更新和当前native formal仍是必需项。之后再交里程碑同SHA全workspace门禁。

## 占位与交付结论

静态搜索C1 WhatsApp及根wecom_callback中的 `todo!`、`unimplemented!`、`NOT_IMPLEMENTED` 无命中；该检查不等于这些模块交付完成。本轮没有关闭模块，没有停用旧闸门或装置。七个空缺适配器是待实现；WhatsApp候选是待验证/待接线；wecomCallback已有实现待本轮正式回归和互动接线核验。没有Cargo/产品/TS运行结果，不能声称任何新增行为已验证。
