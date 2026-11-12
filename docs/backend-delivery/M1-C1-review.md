# M1 Google complete 独立审查（C1 审查线）

本记录只覆盖 Google 非流式 complete 增量及它复用的 Google HTTP/响应边界，不代表 M1 全包交付。审查者只读产品与基线、独占本文；未运行 cargo、容器或产品服务。

- 工作目录：`/Users/da/.codex/worktrees/c1-cli-completion/pilotdeck-rs-cc2c`。
- 固定 TS：`/Users/da/ws/pilotdeck-rs/resources/PilotDeck`，`cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，只读。
- 当前状态：**冻结候选独立静态审查完成，有限 Google complete 包的集中编译与受影响运行结果已核验；正式 Rust CLI 对拍仍待做，不是 M1 全包已交付**。源码行号为审查时快照，最终以各阶段文件哈希为准。
- 本包不包含 invocation 审计，也不能从 complete 局部结果外推 Google 全协议、模型全部消费者或完整 CLI 已交付。

## 已独立核对的基线证据

`tests/fixtures/model-complete-google/generate.mts` 调用真实 `createModelRuntime`，并覆盖 `createNativeSessionTitleProvider`、`createPromptSuggestionGenerator` 和 `classifyAndRoute`。对 `runtime.complete` 的包装记录输入、结果和 HTTP 范围，仍调用原实现；没有用假返回值替代 complete 或 Google SDK。最外端为随机 loopback HTTP。

初版 `_tmp/20261113-m1-google-complete/ts-d/oracle.json`、`ts-e/oracle.json` 与当时入库夹具已逐字节独立比对相同，SHA-256 为 `2466a5b81181070c0a5383a038e0b67d2a9bfaa4834722c3d097f875d768586b`，包含 33 格、35 次 complete 调用、37 次 HTTP 请求。修复 timeout header 后增加向上取整和自定义同名 header 两格，保留原分母，最终为 **35 格、37 次 complete 调用、39 次 HTTP 请求**。最终 `ts-g/oracle.json`、`ts-h/oracle.json` 与入库夹具已独立全量比较且逐字节相等，SHA-256 为 `d643037615b763a0cde3b5770c4a52affb4f03aae172f9682d45c4a72260739a`。manifest 保存四个生产入口的源码闭包、固定提交、驱动及实际 Google SDK 文件哈希；最终两轮另存驱动源码快照。

覆盖正文、thinking/signature、重复及缺失工具 ID、usage、首 candidate、SDK 字段投影、空/畸形/标量 JSON、finish reason、版本化 URL、自定义 header/extraBody、system/tools/media、HTTP 错误与重试、body 中断、提前取消、读 body 取消、provider timeout，以及 stream idle 配置不应套到 complete。标题、建议与 judge 的成功/失败是实际 TS 生产消费者调用；这不是完整 CLI 启动证据。

首败日志保留在 `ts-a/b/c/output.log`：媒体能力漏配、错误使用重试配置字段、误认为 judge 对 HTTP 400 重试三次，均属于装置假设错误。`negative-response/output.log` 已独立阅读：真实 title HTTP 响应改为 `WRONG` 后，生产 title 输出触发生成器明确断言失败。该负控证明消费者结果断言能抓错，不是 Rust 实现 mutant。

最终 `negative-final/output.log` 仍以真实错误 title HTTP 响应触发相同断言。审查者实际执行 `node --test .../check.test.mjs`，6 项通过（1 正控、5 个捕获结果变异）；并实际执行 `check.mjs ts-g/oracle.json ts-h/oracle.json`，全量比较通过。五个变异分别替换 stream endpoint、清空 canonical output、删重试请求、删 timeout header、保留取消后的连接，均被断言拒绝。这些结果变异不是 Rust 产品 mutant。

生成器补齐执行身份后，最终 `ts-i` / `ts-j` 已独立执行 `check.mjs` 全量比较，并确认两轮与入库 oracle 逐字节相等，保持上述 35 格分母与 `d643…` 哈希。两轮 manifest 的 execution 与运行开始落盘的 `execution.json` 一致，记录 command、args、工作树 cwd，以及 HOME/PILOT_HOME/PDRS_BASELINE/LANG/M1_NEGATIVE_RESPONSE 白名单；HOME/PILOT_HOME 均落在各轮隔离目录。两轮及 `negative-manifest` 的 `driver.mts` 快照 SHA-256 均已独立核实为 `9a360b86fb6b32877f1bee97dcd24929cb88131ca9b2e5128e27d207ff17d2db`。`negative-manifest/output.log` 保留真实 HTTP 错标题造成的 `WRONG !== 修复登录按钮` 断言失败，失败轮也有执行身份记录。

正式 TS 入口另有 `formal-b` / `formal-c`：真实 `src/cli/pilotdeck.ts server` 在隔离 HOME/PILOT_HOME、随机端口启动，鉴权 WS 提交 turn。已独立阅读驱动与 identity，并比对两轮 `contract.json` 全等：标题成功持久化；建议作为事件发出但不进入 `read_session_messages.messages`；Google judge 选择 cheap，真实流式请求 URL 为 `/v1beta/models/cheap:streamGenerateContent?alt=sse`；SIGINT 后 exit 0、无 signal。该合同是明确投影，原始 provider 与 Gateway 帧另存 JSONL，不声称完整原始 wire 字节自比。`formal-negative` 由 HTTP peer 返回错误标题，真实正式入口的 titleSaved 断言失败；`formal-a` 首败是把近期 stream.events 误算作持久 messages，原始失败保留。

## 审查发现与闭合要求

| 编号 | 证据与影响 | 当前状态 |
| --- | --- | --- |
| M1-GC-01 | `http/google.rs` 的新 complete 模式最初直接复用 `providers/google.rs:1212` headers，缺少 `x-server-timeout`。现有 oracle 的 `provider-timeout-body-read` 已实际记录该 header 为 `1`；固定 SDK `index.mjs:13633-13634` 在正 timeout 时以 append 语义加入 `ceil(timeout/1000)`。stream 创建 client 时显式移除 timeout，不能一起加该 header。 | 已修并独立静态闭合：`http/google.rs:83-95` 仅 complete 追加，既有 build_http_request 保留大小写不敏感 append/逗号拼接；新增两格已有 TS 实测。Rust 运行待验。 |
| M1-GC-02 | 初稿 `model_complete_google.rs` 比较请求时剥离 mock 的 `clientClosed`，结果匹配不能单独证明底层连接释放。 | 已修并独立静态闭合：TS 两个 hold 场景在 cleanup 前实际观察 response close；Rust `model_complete_google.rs:144-165` 有 2 秒外界限等到真实 peer 的 clientClosed=true，随后完整比较该字段。Rust 运行待验。 |
| M1-GC-03 | 新 complete 的 SDK deadline 必须覆盖 response headers 与 `response.json()`；不能套用 stream idle deadline。旧 stream 仍必须保留 request dump、stream endpoint、idle 与 redirect/parent-abort 顺序。 | 冻结 diff 已复核：旧 wrapper 仍传 stream=true/None；complete 持同一 prearmed deadline 包围 headers/body，旧 stream 的 endpoint/dump/idle 分支保持原逻辑。实际旧测试仍待回归。 |

基线源码依据：`src/model/streaming/streamModel.ts:76-118` 是 Google complete 分支及请求重试，`:350-363` 为真实 generateContent 和错误映射；`:404-418` 明确 stream 不把 provider timeout 交给 SDK。固定 SDK `index.mjs:13428-13465` 建立请求 abort controller/timeout，`:15385-15413` 在 `response.json()` 后附 `sdkHttpResponse` 再投影；`src/model/providers/google/response.ts:10-22,66-149` 定义首 candidate、thinking/tool、工具 ID 与 canonical 输出。

## 冻结候选

以下 SHA-256 均由审查者独立读取文件核对：

| 文件（相对 `src/pilotdeck-gateway/`） | SHA-256 |
| --- | --- |
| `src/runtime/model/http/complete.rs` | `4ef7087e399ab81396c7786cf6afccd6bd9e61f3d71d6877207b25d365ee607a` |
| `src/runtime/model/http/complete/google.rs` | `e4f8906f9e545554137b2fbccc511a5a2118189b42b7065cf8ba9f2dd402755b` |
| `src/runtime/model/http/google.rs` | `acae29461bf8fa66c11b6ddc38516cb77d5f5f4c97678bd0cdaf3ccef72a37f4` |
| `src/runtime/model/providers/google.rs` | `fb827720515652ee1262bd850a87166b656298afc9be59f4ce6ae5f85119e2ff` |
| `tests/model_complete_google.rs` | `f56bf0fdc9b70874e68348ace0209cfd11ff12313b360369bf14226cbb086dcd` |

`complete.rs:665-678` 从 UnsupportedProtocol 进入真实 Google HTTP 请求及既有请求重试策略。新 `complete/google.rs:24-89` 处理整请求取消/超时和 provider 错误；`:93-118` 保留 SDK 对 null/primitive 的实际错误、数组允许投影为空响应；`:141-220` 对照固定 response.ts 保留首 candidate、thinking/signature、工具 ID 去重、usage 与 raw SDK 投影。共享 `providers/google.rs` 只增加 stream/complete endpoint 参数与 sdk_response 可见性。四个产品文件扫描未见 `UnsupportedProtocol`、`unimplemented!`、`todo!` 或 `NOT_IMPLEMENTED`；无转发 TS 的兜底。

## 集中执行前的验证要求（结果见后续附录）

- 统一队列运行 `model_complete_google`，包括 35 格请求/响应重放和实际 native token-saver judge 接线。第一项的标题与建议仅重放 TS 消费者产生的 request，不能称实际 Rust 标题/建议消费者通过。
- 回归 `model_stream_google` 与 `http::google_tests`，保留已有延迟 poll、redirect、parent abort 顺序断言；未运行不得标通过。
- 用 `formal.mjs` 与冻结 Rust 二进制复验正式 CLI/服务装配、标题持久化、建议事件/历史、Google judge 选择及真实 provider URL；已有正式 TS 成功不能替代 Rust 通过。
- 完成取消连接释放、超时与重试验证，并在关闭本局部包前扫描占位与 fallback。完整 M1 的其他协议、恢复/续写、缓存和 invocation 工作保持单独状态。

## 首次集中编译失败与最小修正

根协调者首次集中 check 退出 101，原始日志为 `_build/20261113-042603-h1-first-check/output.log`，其中本包有两处已确认错误：`http/google.rs:92` 把 `String` 隐式转成 `JsValue`，没有对应 `From<String>`；`http/complete/google.rs:143` 保存了指向临时 `JsValue::Undefined` 的引用，触发 E0716。首次失败保留，不能以修后静态检查代替编译成功。

作者在授权窄解冻范围内分别改为显式 `JsValue::String(...)` 和函数局部 `undefined` 绑定。审查者已只读确认代码及新哈希；两处没有改变 header 字符串或缺失 candidate 的行为。其余三个冻结文件及 oracle 未改。当前替代上表对应两项的身份为：

| 文件（相对 `src/pilotdeck-gateway/`） | 修后 SHA-256 |
| --- | --- |
| `src/runtime/model/http/google.rs` | `2689649856e54358a818dbc321de541b99cc7a5fd8dd9441ba3743b05980e6c8` |
| `src/runtime/model/http/complete/google.rs` | `03e0af857a274a251a96f8d51270169b4e1806d698f456be38853950a0a58cd1` |

修后仍待集中 check、专项测试与正式 Rust 入口验证；审查者没有运行 cargo。

## 集中编译与实际运行结果复核

本节更新上一阶段的待验证状态。审查者只读原始日志、identity、源码 manifest 与已生成二进制，没有再次运行 cargo 或测试进程。

三轮集中 check 的退出 101 历史均保留：`042603-h1-first-check`、`043343-h1-second-check`、`043619-h1-third-check`。第三轮暴露 `tests/model_complete_google.rs:45` 的 `url.into()` 同样缺少 `From<String>`；根协调者最小修为 `JsValue::String(url.into())`。该测试文件当前 SHA-256 为 `83a281732452e7975ec64f3aa6090792d8b72d18a48054985b67aeea05d2a11e`，替代首次冻结表中的测试哈希。第四轮 `_build/20261113-043855-h1-fourth-check/identity.json` 退出 0。后续 `_build/20261113-044714-c1-s1-m1-h1-test-compile/` 的 test compile 退出 0，日志有 `build-finished: success=true`，没有 compiler error；这是带既有 warning 的构建成功，不是 clippy -D warnings 通过。

| 实际运行 | 原始证据目录（均在 `_build/`） | 已核结果 |
| --- | --- | --- |
| 新 Google complete | `20261113-045025-m1-google-complete-first` | 2 passed、0 failed、0 ignored。包含 35 格真实 HTTP 请求/响应重放及 native token-saver judge 接线。 |
| 旧 complete 回归 | `20261113-045049-m1-complete-regression` | **9 passed、0 failed、3 ignored**；不是 12 passed。忽略的是三个基线 oracle 再生成测试，本轮未运行。 |
| 旧 Google stream | `20261113-045058-m1-google-stream-regression` | 1 passed、0 failed、0 ignored。 |
| Google HTTP 内部回归 | `20261113-045104-m1-google-http-regression` | 6 passed、0 failed、0 ignored；包括 parent abort、redirect 绝对期限、SDK 错误投影等。其余 5770 项被 filter，不属于此次通过项。 |

三个 ignored 的准确名称是 `regenerates_model_complete_oracle_identically`、`regenerates_model_complete_timeout_oracle_identically`、`regenerates_model_complete_transport_oracle_identically`。相关原始 `tests.list`、`output.log`、`identity.json` 已核对，四次运行均退出 0，并记录各自隔离 HOME/PILOT_HOME。

四次运行共同引用 `_tmp/20261113-c1-targeted-tests/source-before.json`，其 SHA-256 已独立算得 `e930a441dcc9ce02d349fa6020253419f319c91806d59644d5fb19c7d92ede43`。manifest 对应本包修后四个产品文件及上述新测试哈希。`binaries.json` 中四个二进制的实际文件也已逐个读取复算，相同于各次运行 identity：

| 二进制 | SHA-256 |
| --- | --- |
| `model_complete_google` | `f443eb5751aaa87b1699c6fce3e9ef89de980897fea97d02e42501bae65c957a` |
| `model_complete` | `ae78779908976a9238530ecfb3f77e076282680a3698bea4a39f451447f55df3` |
| `model_stream_google` | `77f4cda03d1cb42c9cc7dc6507a0a2c4c1e9d97358f2641fffdab7388b52c5b8` |
| `pilotdeck_gateway` lib test | `3799c4facf2e25141c9f82a29ba0631950ef524230b351c16906a6ef0e2e92ea` |

据此，本局部包的源码审查、受影响编译与上述运行验证已具备证据，可进入本地逻辑提交准备。该结论不包含正式新 Rust CLI 的 `formal.mjs` 回放，也不包含 invocation audit、hydration 或完整 M1 其他工作。S1 独立失败不计为本包通过或失败；完整 M1 不放行。
