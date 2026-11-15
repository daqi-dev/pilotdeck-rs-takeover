# PilotDeck 模型运行时基础

Gateway 源码已改为重导出，原网络仲裁及基础类型定义已移除，workspace 成员与 Gateway 路径依赖已接线。当前候选仍等待 lockfile 正常解析增量及受影响构建测试，不能把源码完成记作“受测的共享执行运行时”。

第一步只包含三个部分：

- `network`：从 `c79787c7e01c112e2aa4da98731828c440d93e49` 的 Gateway `runtime/network.rs` 提取取消、Node timer 归一化、`ExchangeFuture`、header/body/deadline 仲裁及重试等待。算法与语句保持原样，仅将跨包接线所需的 `pub(crate)` 改为 `pub`；来源 SHA256 记录在 `source-provenance.json`。
- `error`：保留原 `ModelStreamError` 三个变体及字段，继续委托 model-core 的借用 `ErrorRef` 处理名称、消息与错误分类视图。Gateway `ModuleError` 转换不进入本包。
- `transport`：原 `ModelBody`、`TransportFailure`、`BodyFailure`、`TransportFuture` 和 `ModelHttpTransport`。保留 Hyper Body 内部适配与 typed failure，未提供发送连接的实现。

本包不含 Hyper client、DNS、socket、TLS、代理、CA 文件、凭据读取、审计落盘，也不包含完整模型 attempt 循环或 ifACE adapter。Node 的事件分类保留在 PilotDeck 运行时内部，不能直接变成公共 ifACE 协议字段。普通 socket 实现使用 `ExchangeFuture::io`；已经完成的结果与有明确时间的测试事件仍使用原来的入口，不能把隐藏的 sleep 包装为普通 I/O 后宣称时序等价。

## 已完成的源码接线

以下接线已经落实；未手工改写 lockfile，也尚未运行本候选的 Cargo 检查：

1. 在 `src/Cargo.toml` 的 members 增加 `pilotdeck-model-runtime`，在 Gateway dependencies 增加 `pilotdeck-model-runtime = { path = "../pilotdeck-model-runtime" }`。新包继承 workspace 元信息；本阶段不增加新的三方版本。
2. Gateway `runtime/network.rs` 显式重导出新包，并保留原有 public／crate 可见性，删除原模块算法。这样 router 的 `AbortFlag` 别名、web/MCP/count 的旧导入路径和既有取消身份继续对应同一类型。
3. 在 Gateway `runtime/model/http.rs` 重导出 `ModelStreamError` 和 transport 基础类型，删除对应的旧 enum/struct/trait/type 定义及 inherent impl。保留 `impl From<ModelStreamError> for ModuleError`：目标 `ModuleError` 仍是 Gateway 本地类型，因此这条转换不需要引入反向依赖。
4. Gateway `fetch_redirect::STRING_TOO_LONG` 重导出本包 `transport::STRING_TOO_LONG`，保留原可见性；其他 redirect 算法暂不迁移。清理原 `BoxBody` 等已无消费者的 import，但逐项检查全文件，不能仅凭迁移片段删除。
5. 保留 Gateway `HyperModelTransport`、encoding、proxy、TLS、审计、dump 和模型循环。完成配置接线与验证之后，才可以声称当前受测产物使用了唯一实现。

新包的直接依赖为 `pilotdeck-protocol`、`pilotdeck-model-core`、bytes `=1.10.1`、Hyper `=1.7.0`（只用默认关闭的 HTTP/Body 类型）、http-body-util `=0.1.3` 和 Tokio `=1.48.0` 的 rt/sync/time。测试增加 Tokio macros/test-util。没有 Kernel 私有依赖，没有 Gateway、agent 或 turn-preparation 依赖。

## 验证范围

新增 13 个独立测试覆盖取消首次事件、迟到轮询时的 response/timeout/parent 次序、prearmed parent 同刻优先级、queued continuation 与 socket 的差异、body continuation 单次优先权、逐次 idle deadline、retry delay／outer deadline 和 drop 后的资源释放。注册顺序负向控制会故意使用不同仲裁入口，要求观察结果随之改变；错误测试保留 Gateway 原 `error_instance_raws_cover_every_js_error_shape_and_no_provider_body` 的输入和断言，并补充 UTF-16、Request details、canonical raw 与 typed failure 保真。

原 Gateway `http_timing_tests.rs` 的 196 行实际 TS 观察矩阵仍在原处，没有用这些基础测试取代，也没有声称已在新包重放完整模型行为。原子接线后应执行新包 tests/Clippy/doc test、Gateway HTTP/Google/原 timing 矩阵，以及共享 network 的 web/MCP/count 相关测试。随后才进入完整异步模型循环迁移。本阶段尚未运行 Cargo。

具体原测试选择：

- Gateway lib 的 `runtime::model::http::timing_tests::`：包含 196 行 provider 矩阵及 4 条同栈 rejected-promise/abort 观察，不能只跑 `http::tests::` 来替代。
- 原 `http::tests::`、`http::google_tests::`、`http::complete_tests::` 和 `http::encoding_tests::`：验证错误转换、两类 provider 路径、Body 类型与解压／EOF／超限行为。
- 原 `--test network_timing`：跨 web／redirect／background 的事件与资源所有权；其 `network_source` 直接引用同一生产模块，保留原观察与断言，避免再次包含重导出文件造成无用 import。
- lib 的 `runtime::mcp::http::network_timing_tests::`、`runtime::hooks::exec::network_timing_tests::`，以及 count_http 的原 3 个测试和 token_accounting 的 `count_fulfilled_promise_keeps_result_through_synchronous_abort`／`count_overdue_response_orders_synchronous_abort_parent_and_timeout`：覆盖其余真实仲裁消费者。
- 原 `fixtures/network-timing/negative_controls.py` 的 12 个 network 变异和 `negative_private.py` 的 7 个 network 变异。两个装置只迁移源码路径，相关装置自测的路径字符串同步迁移，变异、期望与断言不变。19 个锚点迁移前后的匹配次数均已静态核对并记录在 provenance；这不等于运行负控通过。装置要求产品源码匹配 HEAD，因此在候选冻结提交后串行运行，并保留首次失败及恢复结果。
