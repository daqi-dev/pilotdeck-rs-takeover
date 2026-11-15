# Tool / Context 产品能力拆包审计

日期：2026-11-15。用途：P2 公开合同补齐、P3 能力包拆分、P4 正式组装的实施依据。

## 身份与证据边界

- 产品行为起点：backend62 `33b23a773b437aaea33696292810cf67aa54fdef`；本次产品源码基点：`bd87f2e4b29b7de53d907f50d5ab90d755d236a6`。审计工作区 HEAD 为管理提交 `d58dcf408ef66ccedafc912eb7407b101561983c`。本报告引用的 tools/mod、scheduler、builtin/time、cron/schedule、host/modules/tool_context、host/modules/context、context/durable、runtime/context/prepare、agent/ports、iface/resources 已逐文件与 bd87 Git 对象比较，均相同。
- Kernel 源码基点：`c6ed906176897efd5e07442e7a7e0b9537c9b7bd`，ifACE 0.1.8 本地候选。以下 Kernel 行号对应此对象；后续修复不自动覆盖本报告。
- 本报告只做源码/文档审计。未运行 Cargo、未执行组合测试、未读取真实用户 PilotDeck 数据。其他验证作者的结果不由本报告代为认证。
- 本次只新增此实施文档；不改变产品默认后端、不变更前端/TUI、不修改其他作者的实现。拆包、公开注入、正式接线、当前组合实测是独立事实。

产品链接相对本文件；Kernel 链接经过既定 `deps/agent-kernel` 单一源码绑定。若要复现冻结证据，请用 `git show <上列完整提交>:<仓库内路径>`，不要把浮动链接当冻结对象。

## 结论

P3 可以立即拆独立产品能力包，并做一个真实 `get_current_time` 工具垂直切片。完整 P4 Tool/Context 仍依赖下列 Kernel 合同与生产传递补齐；不得将整个 LocalGateway 或原 ToolRuntime 塞进 ToolContract，也不得通过私有共享 mailbox 绕过公开资源调用。

## 现有公开注入与合同

| 能力 | 当前可用形状 | 边界 |
| --- | --- | --- |
| 工具注入 | `ExternalBackends.tools: Vec<ExternalToolBinding>`；binding 为 spec/contract/impl_id/version | 入口已公开，不能再登记为完全缺失；见 [embed.rs](../../deps/agent-kernel/kernel/host/src/startup/embed.rs)，187、241 行 |
| Context 注入 | `ExternalBackends.context: Option<Arc<dyn ContextContract>>` | 195 行承诺默认初始化后保留实现；不代表五操作产品 Context 已接线 |
| ToolContract | effect/register/list/invoke/abort；ToolCall(meta, tool, environment, args)；ToolResult(ok, bytes, unknown) | [tool.rs](../../deps/agent-kernel/kernel/iface/src/tool.rs)，86、126、136、150 行；args 是不透明 UTF-8，Unknown 不得折成成功/失败 |
| ToolEffect | Unknown / ReadOnly / SideEffect，由可信安装方绑定到确切实现 | ReadOnly 要覆盖所有可接受参数、callbacks 和委派副作用；不能由模型参数或产品 `is_read_only(input)` 授权 |
| ContextContract | assemble(session, turn)、rebuild_capabilities(session)、trace_input | [contract.rs](../../deps/agent-kernel/kernel/iface/src/context/contract.rs)，46 行；快照为 bytes + content_type，不等价于产品 ContextRuntime |
| 产品 ResourceCodec | 纯 begin/resume 编解码与受管 cleanup | [resources.rs](../../src/pilotdeck-iface/src/resources.rs)，31 行：不得持有 Gateway、authority、文件或业务 runtime |

## 原行为的所有者

| 领域 | 现有落点 | 拆包后的责任 |
| --- | --- | --- |
| 工具目录/别名、输入校验、结果/错误、串并行策略 | runtime/tools/registry.rs、scheduler.rs | 产品包保留语义；Kernel 保留最终准入、预算、作业、权威结果 |
| cwd/env/session/turn/checkpoint/取消 | ToolContextBuilder、TurnBinding、ToolContextPorts | 可信组装方建立，wire 不得提供 service handle 或换身份 |
| permissionMode/rules/canPrompt/runtime permissions | PermissionDecision 与 scheduler | 产品规则收紧或提供审批信息；Kernel 是最终授权源 |
| hooks、参数改写、file save、progress、nested tools | ToolLifecycle、ToolRunContext、scheduler | 实际副作用受管理；不能借只读工具名称放行 hooks 或嵌套调用 |
| prompt/cache/memory/instructions/spill/compaction 策略 | DefaultContextRuntime、SessionContextState、ContextStorageResources | 产品投影与策略，不建立第二份权威历史 |
| materialization、压缩事实、历史提交 | Recorder、durable context、turn persistence | Kernel 权威记录后产品投影；保留原提交顺序和完成语义 |

精确锚点：

- [tool_context.rs](../../src/pilotdeck-gateway/src/host/modules/tool_context.rs)：202 行 steer remember 不等于 grant，durable ack 后才 apply；256 行 host 构建 cwd/NFKC/PILOTDECK_*；270 行列真实 model/elicitation/audit/file-history/event/subagent/goal/plan 端口；290 行 TurnBinding 禁止用 callback payload 供可信配置；396 行重建工具上下文。
- [tools/mod.rs](../../src/pilotdeck-gateway/src/runtime/tools/mod.rs)：163 行 ToolRunContext，168 行 nested executor 当前递归走原 ToolRuntime。迁移后嵌套调用须重新走 Kernel 受管资源授权。
- [scheduler.rs](../../src/pilotdeck-gateway/src/runtime/tools/scheduler.rs)：273 行 mode gate 先于 schema/permission；345 行 PreToolUse 拒绝/改写与重新校验，allow 不跳过 permission；432 行起先记录 permission audit；581 行 permission updatedInput；602 行才执行工具。
- [prepare.rs](../../src/pilotdeck-gateway/src/runtime/context/prepare.rs)：139 行 SessionContextState 跨 turn 保存 cache/time/storage，211 行释放；835 行产品五操作 prepare/apply/recover/capture/auto_compact。
- [context_storage.rs](../../src/pilotdeck-gateway/src/runtime/project/context_storage.rs)：63、87、104 行分别界定 scoped options、session state 和 session storage；112 行起保留 generation lease 与 scoped spill 隔离。
- [durable.rs](../../src/pilotdeck-gateway/src/host/modules/context/durable.rs)：10 行 prepared context 返回前写 materialization；53 行起有压缩结算不因外层取消消失的旧行为。迁移需要受管结算，不能机械复制 detached task。

## 独立 crate 的最小边界

已有 `pilotdeck-context-core` 保留纯算法。完整边界建议如下；不要求首切片同时创建全部包。

1. **pilotdeck-tool-runtime**：工具定义、目录/别名、JS 校验、错误/结果格式、batch concurrency 与窄产品端口。拆开准备/校验/hook/策略阶段和已准入实际执行阶段；不依赖 gateway、Kernel host 或 Native Loop。首刀也可先建更窄 `pilotdeck-tool-core`，只抽 time 算法，避免搬整个工具树。
2. **pilotdeck-tool-iface**：实现 ToolContract，接受冻结目录、可信 effect 与可信执行上下文 resolver；管理调用取消与 bytes 结果转换。无 LocalGateway、无旧 session manager。ToolOutput 的 content/supplemental_messages/data/metadata 全保留。
3. **pilotdeck-context-runtime**：抽 DefaultContextRuntime、prompt/instruction/memory/cache/tool-budget/compaction 编排，复用 context-core。以窄端口注入文件读取、artifact spill、memory、tokenizer、model、clock、events、persistence；不能以整个 Gateway/Recorder 作为万能端口。session 持有 SessionContextState，generation 持有 immutable factory/config。
4. **pilotdeck-context-iface**：负责公开上下文合同与产品 runtime 的转换/可信身份绑定。当前只能做真实符合 assemble 的切片；完整五操作须先补公开 request/control 面。

组装根构建以上实例，`pilotdeck-iface` 保留 Loop/resource codec 角色。PlanTodo/goal/elicitation/subagent 是独立会话服务与副作用能力；不能为编译临时复制内存权威状态。首包明确不提供依赖这些服务的工具。

## Kernel 实际缺口与任务映射

以下为静态发现或接口能力差额，均不是已实现修复或实测 PASS。最终 ledger 字段由账本维护者按现有 schema 更新，不新增重复交付任务。

| 缺口 ID | 源码事实与所需补齐 | 合同 / 受影响任务 |
| --- | --- | --- |
| TC-01 | Scheduler [resources.rs](../../deps/agent-kernel/kernel/host/src/transport/scheduler/resources.rs):385 将可信 environment_ref 放入 env_id，但 [tool_exec.rs](../../deps/agent-kernel/kernel/host/src/transport/tool_exec.rs):183 的 submit 无环境参数，最终 [service/exec.rs](../../deps/agent-kernel/kernel/host/src/transport/service/exec.rs):225 构造 ToolCall 时 environment 固定 None。须补生产传递；工具还缺有效 deadline/cancel/job/generation/受限 nested client 的公开可信调用上下文。可参考已公开 [ModelExecutionContext](../../deps/agent-kernel/kernel/iface/src/model_session/execution_context.rs):7，不能从 args 重建权限。 | tool/auth/lifecycle；iface.P2.tools-auth、iface.P2.history-lifecycle、iface.P3.tools、iface.P4.composition、iface.P4.loop |
| TC-02 | ContextContract 只有 assemble(session,turn)，Scheduler resources.rs:301–333 不传请求 payload；产品 [ports.rs](../../src/pilotdeck-agent/src/ports.rs):558 的 ContextPrepare 需要 route/protocol/messages/tools/prompts/preview 等。须公开版本化操作+opaque request/response+content_type+能力协商，并绑定可信身份、deadline/cancel 与受管模型/存储依赖。不得用工具名隐藏 Context RPC 或共享 mailbox 偷传 payload。 | context/assembly；iface.P2.model-context、iface.P3.context、iface.P4.composition、iface.P4.loop |
| TC-03 | resources.rs:333 仍 String::from_utf8(snapshot.bytes)，不透明 bytes 未完整透传。首切片可声明只支持 UTF-8 JSON profile；不能声称任意 bytes 已实现。 | context；iface.P2.model-context、iface.P3.context、iface.P4.loop |
| TC-04 | 原 scheduler 的 hook 和 permission 均可改参数；Kernel [policy/hook.rs](../../deps/agent-kernel/kernel/iface/src/policy/hook.rs):12 明确本轮 HookContract 不实现参数修改。须在最终 Kernel 授权前完成受管 preparation，或新增公开 input adapter；invoke 中授权后再改参数不可行。用户 hook 进程本身也需受管。 | auth/lifecycle/tool；iface.P2.tools-auth、iface.P2.history-lifecycle、iface.P3.tools、iface.P4.loop |
| TC-05 | resources.rs:252–299 仍按 binding/context_ref 散列 scope，append 整份 agent.history.v1，read 最后份；缺 expected revision、输入消费原子性与重复提交对账。公开 [CompactionPersistenceContract](../../deps/agent-kernel/kernel/iface/src/context/compaction.rs):77 仅保存既有事实，不替代原 turn persistence。 | history/state-ownership/lifecycle；iface.P2.history-lifecycle、iface.P3.context、iface.P4.state |

## 可以先验证的垂直切片

选择原真实 get_current_time，而不是 toy echo：ExternalToolBinding → Native tool.exec → operation.get → 原 Loop 结果处理 → 下一模型轮。绑定测试时钟，使用临时宿主数据目录。其 ReadOnly 声明只覆盖无额外副作用的精确绑定；不附带任意产品 hooks/spill/nested callbacks。

Context 可先拆包并复用原 fixture 对拍，或做明确限制的 UTF-8 JSON assemble 合同切片；不得把后者称为产品五操作已接通。公开 request/control 面补齐后，先验证 prepare → materialization 先落盘 → model request，再加 tool-result spill 与 compaction。

不建议首刀 read_file：[read_file.rs](../../src/pilotdeck-gateway/src/runtime/tools/builtin/read_file.rs):82 的产品只读标记之外，还依赖权限、文件检查点、多模态、分页、write snapshots；完整依赖未补齐。

## 原行为不丢失的验收

- 保留 [tool_runtime.rs](../../src/pilotdeck-gateway/tests/tool_runtime.rs):533 的 TS 对拍，以及 755、767 行 drop/cancel/deadline 所有权测试；目录别名、串并行顺序、errors、cleanup 不退化。
- 保留 [host_tool_context.rs](../../src/pilotdeck-gateway/tests/host_tool_context.rs):401、696；增加伪造 cwd/env/session/turn/service handles 不生效、未 ack steer 不授权、跨 session 状态隔离。
- 保留 [context_prepare.rs](../../src/pilotdeck-gateway/tests/context_prepare.rs):336 与 [tool_budget.rs](../../src/pilotdeck-gateway/tests/tool_budget.rs):151；增加跨 turn cache/time、scoped spill、过期 spill 不发布、summary reset、materialization 失败时模型调用为零。
- 正式 resident：未绑定/deny/审计失败后端调用零；ReadOnly 放行而 Unknown/SideEffect 拒绝；参数改写后授权；同 turn 多 tool 的 abort/idempotency 不碰撞；nested tool 再授权；在途换绑定保留旧实现；取消/timeout 为 Unknown 且不自动重放；慢工具不阻塞另一 session。
- 历史/压缩：重复 commit、revision 冲突、ack 丢失、崩溃恢复、输入消费顺序、compaction completed 依赖 replacement 真实提交，不只测 append 成功。

## get_current_time 首刀短方案（待分配实现，尚未编码）

1. 新建窄 `pilotdeck-tool-core`，只承载 time 定义/输入输出纯转换与格式化；依赖控制在 std、pilotdeck-protocol、pilotdeck-transport。禁止依赖 gateway、Kernel、cron manager、session/storage 或 Tokio runtime。Gateway 的 GetCurrentTimeTool 保留薄适配，读取可信 clock 后调用 core；不改变已有工具目录/外部格式。
2. 避免隐藏依赖环：当前 [time.rs](../../src/pilotdeck-gateway/src/runtime/tools/builtin/time.rs):14 引用 CronZoneResolverCache，同时 [cron/schedule.rs](../../src/pilotdeck-gateway/src/runtime/cron/schedule.rs):20 反向引用 time::days_from_civil。首刀让 core 接收窄 resolver/host-timezone 输入，原 cache 仍由 Gateway 注入；days_from_civil 可迁入 core 后由原路径薄转发。不要复制 cron cache，也不搬整个 cron 模块。后续独立 iface 绑定可把通用 zone cache 下沉至合适共享层，先明确 crate 依赖再实施。
3. 保持 [time.rs](../../src/pilotdeck-gateway/src/runtime/tools/builtin/time.rs):104–170 的 timezone trim/fallback、finite/TimeClip 检查、trunc、错误文字、JSON content 和 data 双投影；173–197 的扩展年份/负偏移/历法；200–234 的每次读取 TZ、localtime/timezone fallback 仍属于可信平台适配，不能由模型提供 authority。
4. 保留 [builtin_tools.rs](../../src/pilotdeck-gateway/tests/builtin_tools.rs):26 引用的 fixtures/builtin-tools/oracle.json 及既有回放入口，不改 oracle；time.rs:288/328/360/385/410 的缓存重用、大小写归并、失败不缓存、多字节 offset 不 panic、空 timezone fallback 测试继续经过 Gateway 薄适配。先检查测试 seam 可见性，不为迁包扩大生产 API。
5. 第一提交只验纯提取与 Gateway 复用；第二提交才是 tool-iface + resident 真工具链，需 root 在当前 Kernel 验证冻结轮次结束后分配。首提交不假装解决 TC-01/TC-04，不开完整工具能力清单，不切默认后端。
