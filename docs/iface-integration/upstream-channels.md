# 渠道上游增量语义审阅

审阅日期：2026-11-15。本报告只处理 `state/iface-upstream-review.json` 中 `channel-lifecycle-and-persistence` 组的 13 个 `pending_semantic_review` 文件。逐段阅读了生产增量，并追到 backend62 的实际组装入口；没有运行 Cargo、外部渠道、真实账号或真实用户数据。以下“已覆盖”是源码语义结论，不是本轮运行验收通过。

固定比较身份：

- 共同祖先 A：`8a7b0626a068b011c9ed70025c2231f3fb725bd8`。
- 产品起点 B：backend62 `33b23a773b437aaea33696292810cf67aa54fdef`。
- 待审上游 U：`ca7c7cc33db3c42cc81d78239657f3dfbdc422ca`。
- TypeScript 行为基线 T：`OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。

下面引用采用 `B/U/T:path:line`，行号属于相应固定提交，不随接入工作区后续编辑漂移。除明确写出其他目录外，Rust 路径从 `src/pilotdeck-gateway/src/channel/` 起算；T 路径从 `src/adapters/channel/` 起算。U 对象位于 `/Users/da/ws/pilotdeck-rs`，B 对象位于接入工作区。可分别用 `git show <commit>:<full-path>` 复查。没有把上游提交说明或其已接受差异自动当作本轮的批准。

13 个文件中，6 个没有需要迁入的生产修复，7 个仍有应接收的局部修复或必须单独处理的行为冲突。必须按下面的局部结论接入，不能用整文件覆盖。

| 文件 | 结论 | 生产行为与证据 |
|---|---|---|
| `api_server/mod.rs` | 确需接收成功转换语义；异常语义暂缓 | B 与 A 相同。B:293 的数组分支把嵌套数组也变成 `[object Object]`，而 T:`api-server/ApiServerChannel.ts:454` 是 `value.map(String).filter(Boolean)`。例如 `corsOrigins: [["https://a", "https://b"]]` 应得到逗号拼接文本。U:319 使用已有 checked JS 转换器，修正普通数组/对象类型与负零等转换；但把转换错误变成空串并过滤，与 TS 构造时抛错不同。应接收正常值转换，另外处理异常传播，不能称完整等价。B:210 接受显式选项，B:213 的生产环境变量分支仅传字符串，因此常规环境变量配置不触发数组问题。 |
| `bluebubbles/mod.rs` | 显式 stop 所有权已覆盖；丢弃实例泄漏确需局部接收 | U:295、302 将初始轮询和周期驱动持有的强引用改为 Weak，U:884 中断并等待驱动。B:295、299 仍捕获 `self.clone()`，无人调用 stop 时周期任务可一直保活 Runtime。B 的正式 mount 已启用 `owned`（B:896、987），`bluebubbles/lifecycle.rs:5、55` 已跟踪任务、停止接收并回收任务/流，强于 U 只回收驱动的处理。应移入 Weak 寿命管理，保留现有 owned stop 与错误回执，不用 U 的“超时只写日志”替换它。 |
| `feishu/mod.rs` | 确需接收 generation 和回答占用修复；与已有生命周期融合 | B:852 在一个锁段解析 mapper，B:898 另取 generation；这两个锁段之间若 `/new` 改变会话代次，旧映射可能携带新代次进入队列，旧输出逃过代次检查。U:905 把映射与代次同时取出，U:971 让 reset 返回本次新代次。B:640 在 elicitation pending 后直接 spawn 回答，缺少 U:641 的占用 token；两个同时到达的 webhook 可都观察到 pending 再生成两个任务。正式 helper 在 `im_interaction/helper.rs:107` 原子 take，所以不会重复提交同一回答，但第二条消息可能被无 pending 的回答任务吞掉。U 的占用会少生成一个任务，仍选择吞掉该事件，并未恢复 TS 同步消费 pending 后再处理下一事件的完整语义。B:671 的权限回答只有正常 await 返回才清占用，panic 可留下标记。U:587、656、710 加入 drain/回答异常恢复与匹配 token 清理。B 的 `feishu/lifecycle.rs:27` 跟踪任务，不能替代运行中的 batch/回答状态清理。保留 B 的取消和回收机制，分别补原子快照、token 与异常清理。 |
| `feishu/render.rs` | backend62 已等价覆盖 | U:314、406 仅将 visible-failure 集合与 warning 集合查询抽成同一个 helper，集合成员、包含关系、设置顺序和输出字符串均不变。B/A 的重复判断实现等价。无需为此改变产品代码或增加接入阻塞。 |
| `http_client.rs` | 无生产增量 | 所有差异位于 U:180 的 `#[cfg(test)]` 之后：ClockRelease 在 unwind 时释放暂停时钟、探针等待有界化、时间断言容差。这些可作为测试维护单独吸收，不能记为渠道运行能力修复。 |
| `mod.rs` | 无生产增量 | 只有 `ActiveChat::release` 契约注释，强调“检查队列为空”和释放 active 必须在同一临界区；U:88 函数体与 A 相同。B 自有取消/流包装继续保留。该注释不等于已经修复所有调用者。 |
| `runtime_status.rs` | 确需接收 | B:59 的 Arc 闭包不含锁，B:103 直接覆盖目标文件；两个 Tokio worker 同时更新不同渠道时可能读到同一旧快照并互相覆盖，读取方也可能看到截断 JSON。U:69、79 锁住完整 read/merge/write，U:138 临时文件 rename，U:145 使用不会因 stderr 写失败而 panic 的日志入口。正式入口 `native_process/channels.rs:395、404` 共享并克隆同一个 reporter，因此单实例锁可以覆盖实际并发调用。独立 factory 实例或跨进程写入仍需额外协调，不能把这个 mutex 声称为跨进程保护。 |
| `slack/mod.rs` | 正式路径大部分已覆盖；兼容路径与异常关闭需区别处理 | U 修复默认 REST base 双 `/api`（U:50）、auth.test 失败后 reader 泄漏（U:429）、逐帧同步入场（U:635），并补异常 WS close 日志（U:535）。B 旧 `mod.rs:50、351、425、434` 仍有这些形状，但正式 mount 是 `native_process/channels.rs:144` → `slack/native.rs`，不是旧适配器。B native:226 默认 `/api` 配 native:377 的 `/apps.connections.open`，URL 已正确；native:961、994 的 StartupOwner/stop 已清理启动失败；native:459 先 poll 到首次 Pending，native:556 在 await 前原子入场，已保证普通 turn 的帧序。不能把前三项报成正式入口尚未实现。native:423 仍直接忽略 Close code；异常关闭日志可局部补入并用现有 reconnect/owned stop 验证。旧公开适配器仍可补上 URL/失败清理/顺序修复，但不能替换正式 native 实现。 |
| `sms/mod.rs` | 普通行为已等价覆盖；共享 driver 整体迁入暂缓 | U:1735、1862 把 SMS 与旧 Slack 的权限确认尾段及事件流处理抽为带 closure 的共享函数，检查了发送失败恢复、提示 token、事件捕获、错误文本、UTF-16 拼接/trim 和清理顺序，没有新增普通路径修复。B:1904 多了正式停止中的提前返回；U 共享 driver 没有对应 native stopping seam。整段替换会丢掉这项取消行为。U:2034 后是测试探针错误分类，属于测试准确性增量。保留 B，去重可在后续能力拆包时重新设计接口，不作 P0 必须接收项。 |
| `weixin/client.rs` | backend62 已覆盖且边界更完整 | U:53 只把私有无连接超时的 connector 换为模型共享 connector。B:215、227 已改用 `client/network.rs` 的 Network → HyperModelTransport；该 transport 使用 `runtime/model/http.rs:333` 的 proxy connector，其 TCP connect timeout 在 `http.rs:1084` 为 10 秒。B network:14、27、49 还保留按请求和读取阶段的计时语义。应保持 B 的 fetch/代理/响应头后 body 处理，不能还原为 U 的旧专用 HTTP client。 |
| `weixin/live_reply.rs` | backend62 已等价覆盖 | U:229 与 Feishu render 相同，仅合并状态集合查询。warning/visible-failure 成员和逻辑未变，B/A 输出等价，无必需修复。 |
| `weixin/mod.rs` | 确需接收多个独立修复；35 秒 stop 成功语义暂缓 | B 与 A 相同。B:1407 最后一次 shift 与 B:349 的 Drop 释放 active 分两次上锁；夹在中间的新消息入队后无人消费。U:383 的显式 release 在持有 inner 锁时完成 handoff，应接收。B:503、527 凭证读改写未串行化且直接覆盖文件，U:580、596、639 的完整事务锁与临时文件 rename 可防游标/登录信息互相覆盖和半文件。B:561 启动 poll 缺少停止代次重查，U:675 加入 abort 检查。B:2142、2169 的 NativeWeixinGatewayPort 完成任务不删 in_flight，U:2310 以 same_channel 栅栏删除自己的记录，避免长期会话轮换积累并误删新任务，应接收。U 同时改了 home/path 构造 API 和 stop 期限，详见下文，不能整文件接受。 |
| `weixin/state.rs` | 确需接收结果传播与全 key 启动；错误顺序需单列 | B:36 的 in_flight 只广播 bool，B:357 收到完成信号后总返回 Ok，写失败仅发起方得到 Err；并发 flush 等待者可能假报持久化成功。U:40、486 广播相同 Result，符合 T:`protocol/ChannelStatePersistence.ts:65` 共用同一个 Promise 的含义。B:323 顺序 `?` 使前 key 失败后后 key 完全不执行；U:345 的 join_all 启动全部 key，补齐 T:62 的 Promise.all 启动行为。但 U 等所有结果并按排序 key 选择首错，TS 按时间首次 rejection 返回，不能把 U 的错误顺序/等待时长也记为基线等价。 |

需要在吸收任务中保留的限制：

1. **Weixin stop 超时不能成为已清理的证据。** B:`weixin/mod.rs:790` 会等待 poll/recovery 结束。U:939 对两个 JoinHandle 各套 35 秒 timeout，超时仅打日志，随后仍报告 Stopped 并返回 Ok；被 timeout 丢弃的 JoinHandle 不会自动 abort 任务。它确实避免坏 client 无限阻塞 stop，但没有证明任务已退出。这与本轮 Kernel 生命周期/资源所有权验收不兼容。应设计明确的 cancel、join 和失败结果，再接收有界等待，当前先暂缓该成功语义。
2. **Weixin 隔离 home 需要一起改构造调用者。** B 的 `credentials_dir` 只决定 mkdir 与附件根，`credentials_path` 默认独立使用真实 home。只设隔离目录而不设路径会泄漏到真实凭证路径；U:211 改成 `credentials_home` 并由它派生默认 path，解决此陷阱。但 path 类型也从 PathBuf 变为 Option，不能只粘贴构造体。必须检查 CLI/native options 和隔离测试所有调用点，确保显式 path 优先级与默认真实产品路径保持一致。此审阅没有访问那些路径上的数据。
3. **Feishu 回答和 panic 差异需写入本轮账本。** 新的 elicitation 占用会吞掉并发观察到 pending 的后续事件；当真实 helper 已经 take 掉 pending，后续事件仍可走普通消息路径，不能泛称 await 期间全部被占用拦截。上游将占用选择记录为 `A8.3-CH-FEISHU-004-R2`，本轮不能自动继承批准。应该用实际 helper 回答成功、拒绝、panic、取消和 `/new` 交错检查占用释放；只检查 JoinHandle 回收不能证明回答状态正确。drain 的 catch/retry 也要证明不会保留 draining 或形成持续重试循环。
4. **Weixin flush 要区分“都尝试写”与“首错规则”。** 结果共享是直接正确性修复；全部 key 启动避免后续状态静默丢失。上游 `A8.3-CH-WEIXIN-FLUSH-001` 对确定性首错的选择属于额外行为决定。应单独采用或改为基线首拒绝机制，同时保留每个已开始写入的任务所有权，不能用排好序的首错误冒充时间首次失败。
5. **Slack 的 oracle 旧适配器不能替代正式入口验收。** 旧 `mod.rs` 上补齐上游测试仍不能证明 `native.rs` 可用。正式 URL、frame admission、startup 取消、异常 close、重连、owned stream drain 都需从生产 mount 取证。对 native 的源码核对已排除 URL 双 `/api` 与未持有 reader 两项正式入口误报。

推荐后续验收用例（均用隔离 home、环回传输或可控制的注入端口）：

- CORS：嵌套数组、负零、可转换对象和抛错对象，区分成功字符串结果与构造错误。
- BlueBubbles：不调用 stop 丢弃最后一个外部引用后不再周期轮询；显式 stop 仍等待已经接收的流并返回真实清理错误。
- Feishu：用屏障强制 mapper resolve 与 `/new` 交错；延迟回答期间两条输入；回答/批处理 panic 后再发有效消息；旧 token 不得清除新占用。
- Runtime status：两个 worker 同时写不同 key，最终均保留；读者循环解析时没有截断 JSON；stderr 不可写时更新失败路径不 panic。
- Slack：正式 NativeSlackChannel 使用默认 base 生成一次 `/api`；auth 失败与取消都完成 owned cleanup；连帧保持普通 turn 入场顺序；异常 close 记录预期错误并保留既有重连策略。
- Weixin：在最后 shift 与 active release 间强制入队；stop 与 poll restart 交错；并发 cursor/登录凭证保存；会话轮换后 in_flight 有界且旧任务结束不删新记录；超时 stop 不得假报清理成功。
- ChannelStatePersistence：一个 key 写失败时所有等待者返回该失败，其他 key 仍尝试写；人为调换两个失败时间验证首错政策；慢写/取消不得变成假成功。

本次只完成审阅及报告，没有合入上述修复、改动全局审计 JSON 或声称渠道验收通过。P0 可据此为全部 13 项建立明确的接受/保留/暂缓决议；所列产品缺口必须作为实施与验收任务继续跟踪。
