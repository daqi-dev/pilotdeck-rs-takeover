# H1 DingTalk 与 WeCom aibot WebSocket

任务起点：`e8618392e7e4eb24c227eedd28e14c84237ca8a4`。固定 TS：`cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。负责人：本轮 `h1_dingtalk_wecom` 子作者。交付提交：尚无。当前状态：**进行中，私有模块等待首次编译**；本记录不是交付声明。

## 范围与入口

实现目录为 `src/pilotdeck-gateway/src/channel/dingtalk/` 和 `wecom/`，特征装置位于 gateway crate 的 `tests/fixtures/h1-dingtalk/` 与 `h1-wecom/`。不修改 `wecom_callback/`；WeCom 的 aibot 长连接协议与回调 HTTP 加解密渠道分别验收。

每个模块提供 `register`、`register_with_network(mounts,responder,network)`。WeCom 另提供 `register_with_services(mounts,responder,network,command_host)`，正式宿主必须供应原生 `commands::CommandHost`；无宿主的命令操作返回明确不可用错误，不能称正式命令路径已交付。公共 mod、loader、assembly、Cargo/lock 和 TurnRequest 扩展由根作者统一接线。

## 基线依据

- `DingTalkChannel.ts:14` 在 ESM 代码中直接使用 `require`；异常捕获后 `start` 返回无操作句柄。`:72-79` 注册 all-event listener 并连接；`:103-190` 为去重、文本提取、webhook、交互和会话路由；`:192-215` 为 rich text 和有界缓存；`:223-250` 为提交、交互和渲染；`:255-290` 为 HTTPS origin 检查和 UTF-16 20000 单元 markdown 截断。
- `DingTalkSessionMapper.ts:15-37` 为 `/new` 和 `dingtalk:chat=<chat>:general`；`dingtalk-render.ts:4-37` 为文本、工具失败和错误输出。
- `WeComChannel.ts:17-35` 定义 aibot 命令、20s 连接、15s 请求、30s heartbeat 和 5 分钟/1000 条去重；`:312-331` 为订阅和 heartbeat；`:547-670` 为 correlation、policy、interaction、slash/media dispatch 和文字批处理；`:674-763` 为批处理及按 scope 提交。
- `WeComChannel.ts:766-900` 为 mixed/quote/voice/file 媒体输入和 AES 私有缓存；`:918-931` 为 attachments、allowPlanModeTools=false、syntheticMessages 与可选 projectKey；`:1183-1220` 为 MD5、512KiB chunk、init/chunk/finish；`:1250-1310` 为 respond/send/ping；`:1489-1614` 为生成文件提取和敏感路径检查。
- 普通文本经批处理省略 attachments；slash 路径传空数组。这个差别已由真实 adapter mock WS 实测，不将普通文本硬改为 `[]`。

上述文件均位于只读 TS 基线 `src/adapters/channel/`。未复制 TS 产品实现进入 Rust。

## 实测、修复边界与证据

在隔离 HOME/PILOT_HOME 下运行固定 TS 已有构建产物；未访问真实账号、模型或凭据，未修改基线文件。

1. `h1-dingtalk/unavailable.mjs` 不注入 SDK，真实 `start` 返回日志 `dingtalk-stream not installed`，结果 `_tmp/20261113-h1-dingtalk-wecom/unavailable.json`。基线没有锁定/安装 dingtalk-stream；ESM 中也没有 createRequire。正式成功路径在此环境不可用。
2. `h1-dingtalk/oracle.mjs` 仅注入内存 SDK 和 fetch；运行真实固定 adapter。两次输出一致，3 次 gateway submit、3 次合法 webhook，覆盖重复 ID、rich text、origin 拒绝、`/new` 与续会话。保存 `dingtalk-oracle-{first,second}.json`，稳定预期在 fixture `expected.json`。这证明 **adapter mock 契约**，不证明官方 SDK 或真实 Stream 成功。
3. `h1-wecom/oracle.mjs` 注入内存 WebSocket 构造器，运行真实固定 adapter。两次一致，4 次 turn；覆盖订阅、reply req_id、DM/group user scope、allowlist 拒绝、重复、`/new`、quote 和完整 turn payload。保存 `wecom-{first,second}.json`，稳定预期在 fixture `expected.json`。
4. 两个 oracle 加入 `PDRS_ORACLE_FAULT=1`，故意把 assistant 文本换成 wrong，正向断言均失败 exit1；首次错误保存在 `dingtalk-negative.stderr/.exit`、`wecom-negative.stderr/.exit`，未覆盖。

DingTalk 原生化修复候选：以 native Stream 代替不可用 ESM require；明确订阅机器人 CALLBACK 并回复 ack。官方一手协议依据为[钉钉开发者百科 Stream 协议](https://opensource.dingtalk.com/developerpedia/docs/learn/stream/protocol/)，区分机器人回调 topic 和 EVENT `*`，机器人 ack data 为 response=null。参考固定 adapter 的机器人收发类型意图；不把 mock SDK 注入伪装成原版正式成功。根已确认这类不可用/类型意图矛盾属于计划授权 bug 类别，仍需独立评审及正式差异账本登记。

额外下载到 `_tmp/.../sdk/` 的 npm `dingtalk-stream@2.1.6-beta.1` 仅作协议线索；基线未锁 SDK，该源码不属于固定基线，不作为行为事实，也不是产品依赖。

## 当前实现与待验

Rust 已写：DingTalk native Stream 控制 HTTP/WS、callback/system ack、重连、去重、webhook、mapper、interaction 和 owned turn cleanup；WeCom native subscribe/correlation/ping/reconnect、policy、group scope、文字 batch、共享命令 port、入站媒体/AES/0600缓存、上传分片/MD5、工具图片、生成文件及敏感路径检查。无内部 TS 业务转发。

五层状态：

| 层次 | 状态 | 证据与缺口 |
|---|---|---|
| 接口 | 待验证 | 私有 API 和公共依赖已给根；WeCom TurnRequest 扩展由根新增，未编译 |
| 实现 | 待验证 | Rust 已写；尚无编译/产品测试成功证据 |
| 接线 | 待集成 | 根持有 mod/loader/assembly，需正式 config → register → native 的验收 |
| 验证 | 进行中 | 仅 TS oracle 自比/负控实际通过；Rust 单测和真实本地 WS 测试未运行 |
| 交付 | 待实现 | 无集成 SHA、发行产物及正式入口对拍 |

编译前已用工具链 `1.99` rustfmt 检查语法，未自行占 Cargo 槽。建议首次定向验证 `cargo test -p pilotdeck-gateway channel::dingtalk::` 与 `channel::wecom::`；之后受影响 clippy，再正式入口本地 mock 对拍。根调度本机/远端构建，作者不重复跑全量。

尚未闭合：首次编译及所有 Rust 测试；停止取消失败/重试与任务注册表压力；重连、request timeout、媒体上传错误和完整生成文件 oracle；shared command 正式宿主接线；微信媒体大小/错误文案精确对齐；公开 sendImage/sendDocument/sendVoice/sendVideo API；完整入站媒体/出站媒体与正式配置入口对拍；差异登记、独立审阅及发行。已有测试不替代这些必需项。

## 首次集成检查与修批（未复验）

根的第一轮 `cargo check -p pilotdeck-gateway -p pilotdeck-cli --all-targets --message-format=json -j4` 实际 exit101；完整诊断位于 `_tmp/20261113-c1-resume-validation/root-first-errors.{txt,json}`。本包报告到的错误是 DingTalk `headers` 借用临时 `JsValue::Null`（E0716），已改为具名局部值；不能把“已修源码”写成“check通过”。WeCom 当轮仍为未接完整 bundle，不能据没有其错误推断已编译。

本次后续私有修批还补 durable cleanup worker/receipt：停止等待者被取消时实际 drain 继续；下一 stop 复用同一 receipt；失败 stream owner 留存重试；handler panic 永久报告 cleanup ownership unresolved；完成 handler 周期收割；DingTalk ack 写失败进入重连；两渠道 socket 写可被 stop 中断；关闭有资源预算。WeCom auth 与 send 的不同错误文案分别处理，媒体下载超限保留实际字节值。

当前候选测试分母为 **DingTalk 7、WeCom 10，共 17 项**，尚未运行成功。新增每渠道三项分别验证 stop waiter 取消仍保有 accepted turn drain、handler 收割与 panic 不被后续 stop 洗成成功、真实本地 WS 断线重连（WeCom 同 device/new req_id）。这些是组件验证，仍不等同正式配置/发行入口验收。私有源码再次冻结交根复验，作者没有另开 Cargo。

媒体 TS 取证追加：根授权轻量槽后，`_tmp/20261113-h1-dingtalk-wecom/media-oracle.mjs` 在独立 TMPDIR/HOME/PILOT_HOME 运行两次，`media-{first,second}.json` 完全相同。真实固定 WeCom adapter 完成 AES-256-CBC 解密 base64 入站图片（hello，5 字节，image/metadata/安全文件名），工具图片 `x` 的 init MD5/总大小/分片数/文件名、chunk `eA==`、finish 及复用入站 req_id 的图片 respond 均有正向断言。故意将工具图片改成 `y` 后 MD5 断言失败，`media-negative.stderr/.exit` 保留 exit1。此项仍是固定 adapter+内存 WS 的协议特征，并非 Rust 媒体或真实企业账号验收。正向生成的缓存文件已由驱动按精确路径删除，故意失败的隔离临时缓存仍保留作为现场。

持久化补充审查：公共接线作者实读 `protocol/ChannelStatePersistence.ts:28-36`，基线只 JSON.parse，无 shape 验证；`WeComSessionMapper` constructor 只补缺失的 projectByScopeKey。现私有 HashMap 状态不能保证对象插入序，因此在本轮冻结解除后改为 JsObject 并提供 from_js/to_js；不把排序或删除非字符串属性当“兼容”处理。畸形 saved-state 的运行行为尚未取证，必须另测，不能声称通过 shape 验证。
