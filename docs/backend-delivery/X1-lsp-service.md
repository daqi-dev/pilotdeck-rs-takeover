# 原生 LSP service 与工厂接入

本文件描述本轮新增源码；当前只有格式及静态检查，Rust 编译、运行和正式验收尚未完成。固定依据为 `OpenBMB/PilotDeck@cc7c4e6` 的 `tests/cli/project-lsp-composition.spec.ts`、`tests/lsp/*` 和 `src/lsp/*`。原固定五项测试的实际记录见 `_tmp/20261113-lsp-baseline-characterization/summary.json`，不能转写为新 Rust 结果。

## 公开源码入口

`native_process::LocalGateway::start_with_lsp_factory(options, extensions, factory)` 使用正式 LocalGateway 装配。`factory` 实现 `runtime::lsp::LspServiceFactory`，在项目资源代际 stage 时创建 `Arc<LspService>`。factory 向 service 注册 `Arc<dyn LspProvider>`；注册并不执行旧 JS。可执行示例源码位于 `tests/native_lsp.rs`，它使用该公开入口、本地模型协议 mock 和真实工具轮次，并断言下一次 provider 请求带有 LSP 结果。

原 `start`、`start_with_extensions` 不改变默认选择；没有 supplier 时仍是明确的无 provider 服务。该工厂独立于 `NativeExtensions`，因此原扩展 `register/update/unregister` 对新会话的合同不变。没有新增 CLI flag、Node 主进程、动态库 ABI 或原样加载 JS 函数的兼容层。

## 所有权

普通会话共享所选项目代际的 service，不在每个工具调用或每个会话重建共享 provider。真实配置 reload 创建新项目代际；已有会话保留旧租约，直到最后租约释放才 dispose 该代际 service。该工厂选择与固定基线 `lspServiceFactory` 一样属于项目代际，而非扩展注册更新。

只有 factory 显式返回 `session_execution`，或本地受限 sandbox 需要执行绑定时，才调用 provider 的 `bind_session`，并为该会话拥有独立 service。关闭它不能 dispose 同代际的共享 service 或另一个会话的 provider。`register_provider` 返回的 registration 用显式 `release().await` 释放；单纯丢弃返回值不会撤销注册，与基线未调用 release 回调时保留 provider 一致。service 的 dispose 会汇合正在退休的 provider，并保留调用方丢弃 receipt 后的清理所有者；后续 dispose 等待同一完成收据。

清理执行器保留在 service 和 registration 条目中；已接入 Native Gateway 的同步源码消费者在没有当前 Tokio context 时调用并丢弃 release receipt，仍由原 native owner 执行清理。外来短命 runtime 不夺走该所有者。尚未 stage、也从未处于 Tokio runtime 的独立 service 无法启动清理时明确返回错误，不报告成功；之后接入 native stage 可补齐 owner。

单个工具取消传给当前 query 的 `Cancellation`，不通过 dispose 共享服务实现取消。provider 必须拥有并终止自己的 I/O，stdio provider 的 dispose 必须等待实际 child cleanup。原 JS 任意 Error reason 的对象身份不属于该 Rust typed token；取消错误的实际外部映射仍须由独立验证核实，不能宣称已逐对象兼容。

## 请求、执行环境与供给

操作为 definition、references、implementation 和 hover，模型入口仍使用固定基线原名。工具输入保持 one-based UTF-16 坐标，传给 provider 前减一；内部使用 `f64` 保留 JS Number 域，不能通过 `u32` 截断较大合法整数。有限 Number 边界的固定 built-tool 实测与 fixture 见 `tests/fixtures/lsp-number/oracle.json` 及 `_tmp/20261113-lsp-number-witness/receipt.json`。

自定义 execution port 提供 stat、realpath、read、prepare-subprocess 与明确的 URI 映射。其默认 `read_file_bounded` 只核返回长度，不能被描述为具有操作系统级防逃逸或读取分配上限；guest 或其它实现需要自行提供可靠边界。

Native execution port 构造时固定 owned workspace 的原始目录描述符。正常内部 symlink 先解析为 workspace 内目标，实际打开复用 `project_files::open_file_at` 的 NOFOLLOW descriptor walker；从同一打开的 fd 检查 regular/type/size 并限读 `maxDocumentBytes + 1`。不能在检查后重新按原可变路径读取。root 目录改名和路径替换也不改变原目录能力。没有可靠支持的非 Unix 平台明确报不可用，不无声退回路径读取。

stdio provider 是可选的外部语言服务器供给，语言服务器可由调用者指定，不能因此宣称随包携带了语言服务器、Python 或 Node。现有 stdio unit fixture 的 Python 仅为测试服务器。remote provider、持久 server pool、插件 manifest 自动装配和 editor consumer 是固定基线自身未完成的范围，本次不扩张这些项目。

## 待执行验证

新增源码测试覆盖公开 create/model/next-provider 成功、默认错误、较大 UTF-16 坐标、显式 session binding、真实 reload 跨代际、当前 query 取消和释放，以及 provider registration 与丢弃 cleanup receipt。安全读取测试包含真实 barrier、合法内部 symlink、root rename、FIFO、大小限制和取消；真实 stdio `didOpen` 正控与故意不安全的假 canary 泄漏控制也已写入。它们尚未编译或运行，实际结果由统一候选验证更新。
