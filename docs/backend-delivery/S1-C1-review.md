# S1 会话归档恢复独立审查

本记录由 C1 独立审查者维护。审查者没有修改产品、比较器或采集装置，没有执行 Cargo、启动产品、访问网络或真实 HOME。此次只读审查覆盖 `session_archive.rs`、新增 `session_archive_s1_tests.rs` 与 `storage/{mod.rs,transcript.rs,transcript_adapter.rs}`，随后按根授权复审 `runtime/session/factory.rs` 的最小结构化错误入口，并读取作者留下的正式入口证据。范围依据为 `docs/plan/backend-completion.md:124` 与主协调对会话归档原子恢复窄修复的授权；不代表整个 S1 关闭。

- 工作区：`/Users/da/.codex/worktrees/c1-cli-completion/pilotdeck-rs-cc2c`，审查时 HEAD 为 `adcaa3b68ef618632786d970e0a7e28af41e1533`，产品候选为未提交修改。
- 固定 TS：`/Users/da/ws/pilotdeck-rs/resources/PilotDeck`，`cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，只读。
- 当前状态：**集中编译已通过，首次真实单测 3 通过 / 7 失败；测试期望窄修已复审，待重跑**。首次编译及首次单测失败均保留，下文有原始路径和哈希。新产品正式 CLI/RPC 二进制尚未验证，不能称修复通过或产品 parity 通过。

上一轮静态复审冻结候选实算 SHA-256 如下；这是历史候选，编译和测试纠正后的两文件身份另列于下文：

| 文件（均在 `src/pilotdeck-gateway/src/` 下） | SHA-256 |
| --- | --- |
| `native_process/session_archive.rs` | `94e19d3e53317649cf6b045dff7c097cdad7754741f2c7c0cd6323a141f3df0e` |
| `native_process/session_archive_s1_tests.rs` | `80a339e87717b53c95faacb29eb96af7fecc32a2359dafd5a0e41b66e08938c1` |
| `runtime/session/factory.rs` | `cc5a76ceba4113132d1bf1a2cd40a2cb431f6dea732ef2ca7afafb1b5340e44c` |
| `storage/mod.rs` | `965f9947393be72ca1e43256f9c4b08d4e560d9e226780ea0e8b5b45d932ed98` |
| `storage/transcript.rs` | `2c95d2b0b08ffac2fe4482c5dd42bd41048e6534d24ba687b705148692327f16` |
| `storage/transcript_adapter.rs` | `8e9deecfc15b8002efeb23c3d02ddb056ebc521c886826f802cee392eb5d5a8a` |

## 初版发现及处理要求

| 编号 | 证据与影响 | 闭合要求 |
| --- | --- | --- |
| S1-R-01 | 初版 `session_archive.rs:340,377,419,453,476,480,491` 将 I/O / LedgerError 统一转为 `RequestError::new`，而该构造器在 `runtime/connection/mod.rs:75` 固定 `gateway_request_failed`。TS 正式 RPC 中受控 append 失败保留 `EIO`；旧 Rust 返回 generic | **源码层已修正、运行待验。** 新版 `session_archive.rs:337-368` 以 `LedgerError::Io` 分流；优先使用已有 Stage，无 Stage 时仅用真实 errno 查 code、保留原文字。直接 lstat/fsync/link 边界提供实际 syscall/path；不复制注入器文案，不解析文本猜 errno。冲突及归档校验专用错误码保持 |
| S1-R-02 | 初版取消单测只在 caller abort 后验证 `claim_restore` 冲突，不能单独证明 writer lease 仍被 blocking writer 持有；提前释放 lease、仅保留 RESTORING 的错误实现仍可能通过该断言 | **断言已补、运行待验。** 新版取消测试在 accepted_input 屏障仍持有时，直接调用非阻塞 `try_writer_lease(target)` 并断言 None，再释放屏障、重试并验证完整 transcript；不靠 sleep 推断 lease 有效 |

根授权后的 `factory.rs:275-301` 将原实现抽成 `try_writer_lease_storage`，返回完整 `LedgerError`；旧 `try_writer_lease` 仍按原签名转成 `RuntimeError`，旧 caller 的行为和 mutex/flock 取得顺序不变。restore 使用新的结构化入口，已核对没有复制另一套锁算法。既有 `LedgerLock::acquire` 对 Rust-only lock open 错误主动包装成不带 errno 的文字，这个旧边界未改；新版如实保留该诊断，不能宣称所有历史 storage 错误都已有 Node 格式或结构化 errno。

新增测试源码共 10 项：中段 EIO 清理并重试、caller 取消后的 reservation/lease 持有与重试、sanitize 别名竞争、现有 writer lease 冲突、stage 期间外部目标抢占、普通文件/目录/活与悬 symlink 占用、真实祖先非目录 ENOTDIR 后修复重试、真实目录权限 EACCES 后恢复重试、真实 link 源路径消失 ENOENT 后重试，以及非 I/O 错误不误标 errno。link 失败测试在真实 Jsonl store 完成 accepted_input 后移除本次 stage 名字，保留已打开 inode，让实际 hard_link 失败，并非只伪造 RequestError。首次真实运行结果及测试期望纠正见下。

## 集中编译及首次运行的复核

审查者只读取根统一执行留下的材料，没有自行运行 Cargo 或测试二进制。

| 批次 | 已核对结果与身份 | 结论 |
| --- | --- | --- |
| `_build/20261113-042603-h1-first-check/` | 首次 gateway all-targets check 退出 101；identity SHA `e4ff8fd19189d4ed0e9d736f1bb454ff9a771d9665b3812361c55e65055b4116` | S1 自身错误包括对 JsString 调用不存在的 `push_str`/`contains` 及跨类型等值断言。不能用后来成功覆盖首次失败 |
| `_build/20261113-043855-h1-fourth-check/` | `cargo check -p pilotdeck-gateway --all-targets` 退出 0；HEAD `ef3249ce3d815b8ece1feb37bac18690fdc5116f` 加冻结 WIP；identity SHA `191165220416c4b85465089e145cd1a233ecd4fe182261afdf7f7b2b93940e7d`，output SHA `1d92d510799389a92aeca5a3521ee163ae86e3cab1504afa0ffd7a0fcb9daedd` | 已编译通过，不等于运行通过 |
| `_build/20261113-044959-s1-first/` | 10 项，退出 101：3 passed / 7 failed / 0 ignored，4 test threads，`permissionFaultsRequired: true`；identity SHA `f59e0fd0b20c6c735795b5c71fc808ae07b578612bd3c5ce455bcf7bc1ec3773`，output SHA `d7ea277cd356012dabdb2ad0cd2eb71306d3af27f979c6fe38b0acd7d467a25e` | 首次真实测试失败，原因独立核对如下，仍须重新运行 |

编译修正只涉及两个拥有的文件。产品 link-error 后缀改用现有 `JsString::units()` 与后缀 `encode_utf16()` 拼接后 `from_utf16`，没有经过有损 String 转换；测试用 UTF-16 units 匹配、JsString 等值或 `equals_str`。复审实算产品 `session_archive.rs` SHA 为 `14cc377bc2aa8f33f16ea6364d8b3833b0df88d2a72fe1184cb48fe4e73b27bf`，编译修正版测试 SHA 为 `8230f43355ec85b4d915442d49354d0bf1eee2c099548bc50567dfb6acf8443f`。`candidate-manifest-v2.json` SHA `b440d838154ca712041f5f51f13232d0fa7925c385237512c982ec42eb2dcc48` 保留其余文件身份。

首测 receipt 绑定测试可执行文件 SHA `3799c4facf2e25141c9f82a29ba0631950ef524230b351c16906a6ef0e2e92ea`、源码清单 SHA `e930a441dcc9ce02d349fa6020253419f319c91806d59644d5fb19c7d92ede43`。通过的三项是非 I/O 不误标、已有各类目标不变、stage 期间外部目标不覆盖。失败的七项全部落在当时 `session_archive_s1_tests.rs:154` 的最终 `portable_archive(...) == archive()`：实际对象顺序为 `schemaVersion,format,messages,title`，测试错误地复用 restore 输入的 `schemaVersion,format,title,messages`。前置断言到达该点不代表整项测试已通过。

独立依据不是 Rust 当前输出：`ts-verified-1` 和 `ts-verified-2` 的真实成功 export 及重启 export 都是 `schemaVersion,format,messages,title`；固定 TS `src/cli/createLocalGateway.ts:1643-1647` 同序。作者新增单独 `expected_export()`，保留 restore 输入中 title 在 messages 前的顺序，用 TS 实测顺序构造输出期望；`assert_complete` 仍使用严格有序 `JsValue` 等值，行数、条目类型和链诊断断言不减。审查者已核对新测试 SHA `8e602ee333fa2ec2c6d6fc5d568dc153d565222e3b433c4e2b03db845998ea18`；产品仍为上述 `14cc...`，没有为测试变绿更改 export 行为。本窄修源码复审通过，真实重跑结果尚待根反馈。

## 生命周期与文件发布审查

正式调用链为 `native_process/methods/history.rs:41` → `Requests::restore_session_transcript` → `restore_validated` → blocking `write_restored`。固定 TS 对应 `src/cli/createLocalGateway.ts:1315-1369`：写独立 `.restore.tmp`，记录 title 后记录 accepted_input，最后 rename；异常删除自己的临时路径并重新抛出原错误。

复审版 Rust 的资源归属符合预期（主体生命周期与初版一致）：

1. 原始 session key 的 `Restoring` 在归档校验前取得；目标路径 canonical 后取得共享文件 fence。不同原始 key 经 sanitize 落到同一 transcript 时，共享该 fence。
2. 第二个 blocking closure 同时拥有 `Restoring`、fence 和既有 cold writer lease。caller future 被取消时仅置原子取消标记，不能释放这些仍由 blocking writer 使用的资源。canonicalization 阶段或等待 fence 阶段取消不会留下半份最终 transcript。
3. 临时文件使用既有 `create_temporary` 的唯一私有 sibling、create_new 与 RAII；既有 `TranscriptWriter::for_store` 把两段 JSONL 都写到临时路径。`write_restored` 返回或失败时先清理其局部 writer/temp，再退出外层 closure 释放 lease、fence、Restoring。因此未发现旧清理在后继 writer 启动后删除最终文件或清掉新 reservation 的路径。
4. 中段写入及 `sync_all` 后检查取消；同目录 `hard_link` 发布完整 inode，目标已被外部创建时明确冲突，不退回覆盖 rename。`symlink_metadata` 在 canonical 前检查叶子占用，目录、活 symlink、悬空 symlink 均不会被当成空目标。
5. 最后一次取消检查与 link 之间仍存在正常的提交竞态：结果允许“无最终文件”或“完整最终文件”，不能承诺此时取消必定无提交或必定可重试。SIGKILL 无法执行 RAII，可能留下独立临时文件；不把孤儿 stage 扫描删除冒充安全恢复，也不宣称已证明掉电持久性。

三处 storage 文件只增加 crate 内可见性及 `Temporary::path` 访问器，未修改既有 writer 的序列化逻辑。JSONL 仍使用插入有序 `JsObject` 和原有记录方法；消息、metadata、sequence/parentEntryId 的构造没有新增另一套实现。最终字节及键序仍须由新 binary 的正式场景验证。

目录/悬空 symlink 一律冲突，以及外部 creator 抢占后拒绝覆盖，是比固定 TS 的 stat/rename 更窄的安全行为，不能标作这些格与 TS 完全相同。作者 README 已说明该区别，根新增 `specs/gateway-parity/diffs/S1-RESTORE-NO-REPLACE-001.md` 登记窄差异；它不免除中段故障、普通成功、重启及错误来源的实际验收。

## 已独立读取的正式证据

证据目录为 `_tmp/20261113-s1-session-restore/`。以下都是作者运行后留下的材料，审查者没有重新运行产品。

| 材料 | SHA-256 | 独立核对结果 |
| --- | --- | --- |
| `ts-final-1/observed.json` | `0ef0c45cca1ddb26455f745120a29809d04c32c053f967aad3a664b77b681c5f` | 6 段操作、3 进程、0 provider 请求；普通进程退出 0，中段终止为明确 SIGKILL，无兜底 forceKilled |
| `ts-final-2/observed.json` | `0427f5a7646dbf39bb98b94f7ca1446d0b31edb9118e465252fc5ea6d02e8ed7` | 同一采集器及 8 个 source/dist 哈希；同样 6 段、3 进程、0 provider 请求 |
| `rust-before-1/observed.json` | `e7df5df3a42c973e958a7fd3e1f270727b48222c69d2e646ae45753bdd23064b` | 固定旧 binary 在中段 EIO 后留下仅 metadata 的最终文件；同 key 重试冲突。中段 SIGKILL 后重启 export 报 archive message count 无效，重试仍冲突 |
| `rust-before-negative-1/observed.json` | `5ac2f489c4cca88ab29b1b14de6939462570995011140a182ceda31dd2bc3cd8` | 当前默认 safe 断言实际拒绝上述旧 binary；首次错误为最终路径本应不存在却存在，不是只改结果 JSON 的合成负控 |
| `rust-before-negative-1/failure.json` | `09453c7b6a423254e6809c49a3f6ad6d6e07d8bebae0d10a84af155c229db195` | 保存首次断言失败 |
| `negative-control.json` | `1a6d399bbc50d528b7ec56c74c86920dde9a6a681bf4254ceffe370cade28970` | 记录默认 safe 命令退出 1、半文件确实存在、retry 冲突 |

最新 TS 两轮均确认：第二段 accepted_input 写前注入 EIO 时最终路径不存在、临时文件清理、同 key 重试成功；有标题/无标题成功，已存在文件不变；正式 export 返回原 archive；重启读取一致；metadata 已写入临时文件后 SIGKILL，重启仍无最终会话且可重新恢复。

审查者独立检查两轮成功 JSONL 顶层键序，分别为 `type,metadata,sessionId,turnId,sequence,createdAt,entryId,parentEntryId` 与 `type,messages,metadata,sessionId,turnId,sequence,createdAt,entryId,parentEntryId`。两轮稳定业务投影也相同：仅去掉 RPC id，按字段处理 entryId/parentEntryId、turnId、createdAt/updatedAt；保留消息、metadata、错误码、序号、文件存在状态、导出与重启结果。此检查没有比较故障文案、run stdout/stderr 或全部 hold 临时字节，不能冒称完整输出 parity 比较器。

采集器 `capture.mjs` SHA 为 `834641a2afc5c13eed10b4686137ba00d5d87595f2aa2c00bd9c4b99b33ca70c`；`ts-append-fault.mjs` 为 `cc6748d4d1ac93ec6b370a274df558939b8fa014ee84f5435466728956bd7d17`；`metadata.json` 为 `59b34db471471c90841c54fdcdeac935a45a4eaf434c3c5f573793b46d3f6fac`。审查者重新哈希了 metadata 指定的 8 个只读 TS 文件，全部匹配，两轮记录的 map 相等。故障注入器仅针对隔离 PILOT_HOME 下的 `.restore.tmp` accepted_input append；EIO 是受控注入，不能把其自定义文案当成自然内核错误的测量值。

### 后续七段正式 TS 证据

随后作者新增真实目标占用 RPC 取证，最新 `capture.mjs` SHA 为 `d1dc8a7b46ce890158f67dba2c551f118d9942d47655310750b754a7b00db3db`。审查者已读取 `ts-verified-1/observed.json`（SHA `521d30e0e90eac8ba2ee0af3f27894079a229972f4b81fac7dc14ee9614e68d7`）和 `ts-verified-2/observed.json`（SHA `16badfc5969b60cf333c3f7930834e13969c2839f78f1fb7cf27980a1c280d7c`）：各 7 段、3 进程、0 provider 请求。新增五格的两轮实际结果一致：目录返回 EISDIR 并保留；活文件 symlink 返回冲突并保留链接和 referent；悬 symlink 和活目录 symlink 被 TS rename 替换为完整 archive，而 referent 不变；stage 后外部创建的最终文件被 TS 覆盖。该结果支持登记的窄安全差异，不能用“TS 同样拒绝全部占用”概括。

当前默认 safe 驱动又运行固定旧 binary，`negative-control-final.json`（SHA `e44823af37cf929eae96e7b385535b4cdce7183fcff6d59a887d7aa0b8e744cd`）记录退出 1、半文件存在、retry 冲突；对应 observed SHA 为 `7b20e320ebd42fe04bb4620acca8f334789b680b4bb78911aea0e375e611fbdf`。这仍是实际产品负控；旧 binary 的源码回执限制不变。

作者 `self-compare.json` SHA `9d4727835dc9919b99254e6344a6305812d9f2c286ea351a6af4e4f6a9db8b4a` 记录七段自比相同。独立审读 `compare.py` 后发现其证明范围较窄：JSONL 解码后仍回写成字符串，因此保留 JSONL 键序；一般 RPC/export 对象使用 Python dict 等值，不保护对象键序。UUID 正则会作用于所有字符串，包括业务 text；path 归一只留 basename。因此该脚本不能作为通用“全部业务值和完整输出均严格保留”的证明。当前固定场景的业务消息不含 UUID，真实 export 顺序已从原始观察独立确认；已通知作者和根收紧归一化并显式保护 RPC 键序，不能据此放松 Rust 单测的有序断言。

## 证据限制与剩余验收

- `pilotdeck-before` 独立实算 SHA 为 `7ba4eabc0738974f7c80a2d37062ff56f90c740e66eab69d942472552fa38f59`，与 `before-binary.json` 一致；该 receipt 本身 SHA 为 `bdd0ad2f4d66b6d0947b7eaf10ef3d2feb0022562a1dc114835b5939ca0f46f9`。未找到将此二进制绑定到源码提交及构建命令的回执，因此只能证明这个固定旧副本的行为，不能称它是当前 WIP/HEAD 的修前构建。旧 observed 的 `revision` 字段写的是 TS 基线，不是 Rust 源码身份。
- TS 两轮固定 Node `v22.22.0` 与 8 个 source/dist 文件，并检查 tracked src/packages 无修改；未记录 Node 可执行文件、package/lock、注入器在每轮结束的完整身份闭环。现有证据足够支撑此次狭窄行为观察，不能借此宣称整个执行环境的完整供应链证明。
- 进程 SIGKILL 与请求 future 取消不是同一场景。前者已有 TS/旧 Rust 正式证据，后者已经在首次 Rust 单测执行到最终 export 比较处，但整项因错误期望失败；取消期间 lease 的直接断言不能单独冒充整项通过，须随窄修重跑。
- EACCES 测试在特权环境无法触发权限拒绝时会明确 SKIP。正式权限验收应设置 `PDRS_REQUIRE_PERMISSION_FAULTS=1`，或将该格如实保留未验证；不能把默认进程退出成功当成真实权限场景已通过。
- 新 Rust 已有集中编译和首次单测失败回执；主协调须重跑固定窄修候选，再用固定新 CLI binary 运行同一正式 safe 场景，保存源码/二进制/命令身份并保留失败首报，核对中段失败清理、取消后重试、普通成功、已有目标不变与重启读取。以上未闭合前，状态保持待验证。
