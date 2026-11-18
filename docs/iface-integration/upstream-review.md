# PilotDeck-RS 上游增量审阅

本报告为 ifACE 接入 P0 的源码审阅记录。**原 59 个待语义审阅路径已完成逐项审阅，P0 仍未关闭。** 三份报告明确了已覆盖、同义抽取、无生产增量、范围外、需局部接收及行为冲突；其中真实产品缺口必须继续实施和验证。审阅完成不等于接入完成，旧上游通过记录不计入本候选验收。

## 固定身份与变更规模

- 产品起点 backend62：`33b23a773b437aaea33696292810cf67aa54fdef`。
- 原计划 main：`a45c9403edb45a0a53a518f9bb5ea709b1ac7268`。
- 本轮远端更新后审阅 main：`ca7c7cc33db3c42cc81d78239657f3dfbdc422ca`。
- 共同祖先：`8a7b0626a068b011c9ed70025c2231f3fb725bd8`。
- 固定 TS 基线：`cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。

main 自共同祖先改变 448 个路径，其中 340 个在 `src/`；backend62 同期改变 833 个 `src/` 路径，两侧相交 81 个。原计划 main 到当前 main 的差额为 12 个路径，其中 10 个 `src/` 路径全部是测试装置，未新增生产代码，也未改变冻结判分文件。生产候选初筛的 62 个路径包含内嵌测试和支持代码，不能解释为 62 项产品功能。

逐路径原始、backend62、main blob 身份及处置见 `state/iface-upstream-review.json`。本次归并保留全部 448 路径与既有 blob/提交身份，不改来源 catalog hash。保留 `path_inventory` 的流程文档和未接收测试装置仍仅表示盘点；原 59 个生产候选已经替换为实际 hunk 语义审阅深度，并记录报告锚点、行号和表格行。

## 三份审阅与路径计数

- [渠道审阅](upstream-channels.md#渠道上游增量语义审阅)：13 个路径；追到 backend62 的正式 native 组装路径，区分旧公开适配器与生产入口。
- [基础能力审阅](upstream-foundation.md#逐路径处置)：18 个路径；配置、watcher、HTTP、字符串兼容与历史投影，包含固定 TS 源码对照。
- [运行时审阅](upstream-runtime.md#逐项接收决定)：30 个路径；其中 28 个原待审路径，另包括已处理 services 和测试 manifest。涉及消费者时与基础能力报告交叉核对。

下列计数是原 59 个路径的互斥主处置；同一路径的混合 hunk 在 JSON 的 `hunks` 中另外记录，不能把“某个窄补丁已写”解释成整个文件的增量全部接收。

| 原待审路径处置 | 数量 | 含义 |
|---|---:|---|
| backend62 已覆盖 | 5 | 正式实现已有相应能力或行为；源码等价结论不是运行验收 |
| 同义抽取暂缓 | 7 | UTF-16/helper 或共享 driver 去重，无新增普通行为缺口 |
| 无生产增量 | 10 | 只改测试、注释或负向控制注入；不为此改生产所有权 |
| 范围外暂缓 | 1 | config binding 的 StaffDeck/SOP 特定错误类，保留交接 |
| 行为冲突暂缓 | 7 | 会话身份拒绝和 cron 失败停止政策，需要明确合同 |
| 窄补丁已实施、待本候选验证 | 3 | AlwaysOn、automation、assembly slot-before-poll |
| 需局部接收、尚未实施 | 26 | 渠道、watcher、HTTP、错误类、历史畸形输入及日志安全缺口 |
| **合计** | **59** | **未读/待语义审阅为 0；其中 36 路径仍有接入开放事项** |

全 448 路径的主处置另含流程/文档 108、既有测试装置 277，以及先前已实施的上传/附件 4 文件。于是全表“窄补丁已实施、待当前组合验证”为 7 文件；其中前 4 文件已有先前捕获产物的局部通过证据，新增生命周期 3 文件尚待测试。文件计数不是功能完成率。

## 接收与暂缓的具体边界

| 增量组 | 当前处理 | 必须保留的事实与剩余工作 |
|---|---|---|
| 上传 metadata、附件校验、lease 清理安全 | 已实施窄子集，前一捕获产物测试通过 | Unix 非阻塞单次打开、上传工件叶 symlink 拒绝、同 fd 类型/身份核验、精确长度读取与增长拒绝。当前组合仍需随最终候选验收。 |
| 普通路径附件 composer | 已按 backend62 重适配，前一产物测试通过 | 保留 TS 的普通项目内外路径及正常 symlink；O_NONBLOCK、同 fd 类型检查、原有限额 cap+1；main 旧 lease-root 限制不接收。 |
| JSON 元数据预算与附件二进制大小 | JSON 64 MiB 预算已实施且局部通过 | metadata/lease 同预算，超限 fd len 在分配前 EFBIG/read，写侧 temp 打开前 EFBIG/write；二进制配置不变，1 GiB 固定上限不接收。不可读 list 记录跳过也未接收，避免改变计数/幂等/错误语义。 |
| 渠道生命周期与持久化 | 13 路径已审；7 路径仍有局部缺口 | Feishu 原子 mapper/generation、回答占用及 panic 清理；BlueBubbles Weak 寿命；runtime status 事务锁/原子写；Weixin 队列移交、凭证事务、隔离 home、poll 停止重查、in_flight 清理和 flush 结果共享。正式 Slack URL/启动所有权/入场顺序已有覆盖，异常 Close 及旧公开 adapter 另补。 |
| 渠道行为冲突 | 明确暂缓，保持开放任务 | 不接受 Weixin 超时丢 JoinHandle 后返 Stopped/Ok；Feishu 占用时并发消息吞掉政策与 Weixin 排序首错均需本候选合同，不能继承上游差异批准。 |
| 配置 watcher 与错误类 | 已审，需局部实施 | 通用配置错误 name 保留；watcher join panic 分类、content-unknown twin 防止伪 reload；StaffDeck/SOP binding 特定入口范围外。 |
| HTTP flush/日志安全 | 已审，需局部实施 | 未 flush 响应尾部仍须阻止新 header 读/408；proxy 文件也是正式 Native HTTP 的共用连接实现。坏 stdout/stderr 的日志不能 panic，需要覆盖 CLI、宿主、维护、TLS、预算及清理路径。 |
| 会话身份与管理方法 | 明确冲突暂缓 | 孤立 surrogate 经 lossy 路径会别名到 U+FFFD 会话；拒绝需统一所有读写/delete/rewind/turn 入口并保留当前错误顺序。main projectKey nullish fallback 会回退 backend62 的准确 dialog_project 行为，不接收。new_session UTF-16/JS truthiness 已覆盖。 |
| 历史与 UTF-16 | 正常能力已覆盖；畸形 artifacts 差额待补 | backend62 已有正式 session_projections/web_messages 消费链；code_point_chunks 和 some 检查抽取同义。length/filter/spread、字符串 artifacts 等差额补现有投影，不新增第二历史事实所有者。 |
| AlwaysOn / automation | 三文件窄补丁已实施待验证 | 单一任务实际持有排空，所有 stop 等同一完成值；attempted 保留被取消 start/stop 的补偿义务；cleanup slot 先于 start poll。未接收 main 的 abandoned 重入或失败吞掉的一次闩锁。 |
| Cron / MCP | Cron 冲突暂缓；MCP 默认所有权已覆盖 | Cron 首停失败后再停返 Ok 不能当排空证明；MCP 继续持强类型 RuntimeLease，不为 mutant 改为可缺席 Option。 |
| 测试装置与管理文档 | 保留来源，不整体合并 | 直接行为测试可逐项迁入；不继承与 main 树绑定的摘要、行数、冻结分母和通过记录。 |

## 实施和实际验证边界

上传/附件安全实际修改四文件：`upload/mod.rs`、`upload/tests.rs`、`native_process/services.rs`、`native_process/services_tests.rs`。新增 13 个上传用例（含 4 项 JSON 预算）与 4 个 composer 用例；旧 symlink 攻击用例验证更早的 ELOOP，原 hardlink/rename 窗口继续核验句柄 containment 和 inode 身份。

已核对主协调保存的 `_tmp/20261115-iface-tests/captured-gateway-receipt.json` 与对应日志：捕获测试二进制 SHA-256 为 `f53ebc94dfe7dd323b8b326ec9815badcccba8397e756e794178fde9aadc4abf`，upload 为 **86 passed / 3 ignored**，services 为 **54 passed / 2 ignored**。运行前后产物 hash 相同。receipt 明确该产物早于 turn-preparation 和 AlwaysOn 修改；这些结果只证明该捕获版本的相应用例，不能证明后来生命周期补丁或当前完整组合通过。

后续生命周期三文件补丁的实现和五个新增回归见 [runtime 交接](upstream-runtime.md#本轮窄补丁交接)。本实现者仅完成定点 rustfmt 与 whitespace 检查，没有另行 Cargo；新回归与正式 native assembly 初始化失败清理待主协调固定候选验证。不能用 provider 直接测试代替真实宿主入口验收。

固定 TS 的隔离探针保持原件：

- `AttachmentResolver`：普通 symlink 成功；stat 采到 5 字节后文件增长到 1,004,096 字节，仍输出超默认 1 MB 文本；FIFO 在内外两层 watchdog 后由本探针记录的 PID 回收。原件 `_tmp/20261115-iface-upstream-baseline/run-k_7ivs3a/receipt.json`。
- `UploadStore.create/get`：普通记录成功，metadata 替换为 FIFO 后挂起并由 watchdog 回收。原件 `_tmp/20261115-iface-upstream-baseline/upload-jzcs7kzb/receipt.json`。
- 首次 upload 探针漏传 relativePath，收到 `UPLOAD_MANIFEST_INVALID`；它是装置 INVALID，保留在 `upload-7wjdu4g7/receipt.json`，没有当作产品失败或覆盖首次记录。

安全差异只新增 `specs/gateway-parity/diffs/IFACE-UPLOAD-SAFE-HANDLE-001.md` 与 `-R1.md`；未改已有差异文件、冻结判分文件或源 catalog hash。

## P0 出口

已闭合的是身份/路径盘点及原 59 路径的语义审阅。已明确覆盖或同义的事项不再伪列为未读；范围外和行为冲突都有具体原因。未闭合的是接收候选的真实产品缺口、生命周期当前候选测试、冲突合同与最终联合验收证据。

P0 保持进行中。协调者应把渠道、watcher、HTTP、历史畸形输入、非 panic 日志以及会话身份/停止错误语义分配为独立实施与验收任务；不能仅因清单无 pending_semantic_review 而宣布产品源码收敛或 ifACE 接入完成。
