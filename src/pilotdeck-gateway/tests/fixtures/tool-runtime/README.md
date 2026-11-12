# 工具运行时场景夹具

`oracle.json` 来自固定 TS 基线（`cc7c4e6`）真实运行：用确定性的假工具驱动真实的 `ToolRegistry`、`ToolRuntime`、`ConcurrentToolScheduler` / `SequentialToolScheduler` 与 `toolSchedulerAdapter`，用虚拟时钟录下结果、事件与拒绝消息。`sourceSha256` 覆盖这五个入口的相对 import 闭包。

```sh
PDRS_BASELINE=/absolute/fixed/PilotDeck node --import /absolute/fixed/PilotDeck/node_modules/tsx/dist/loader.mjs src/pilotdeck-gateway/tests/fixtures/tool-runtime/generate.mts /absolute/output-parent
```

每次运行在输出父目录下用 `mkdtemp` 新建独占目录，`flag:'wx'` 写入。连续两次输出字节一致才更新正本；正本只追加场景。

场景字段：`schemaJson` / `inputJson` 是双方都按 JSON 文本解析的原始输入（保留自有 `__proto__` 键和未排序的整数键）；`schemaPrototypes` 在解析后给指定路径的对象设显式原型（null、数组或对象），模拟程序化或合并出的 inputSchema。

## 并发分组与并发阶段首个错误（2026-11-12 起与基线一致）

TS `ConcurrentToolScheduler` 只看 `isConcurrencySafe`；基线的 `agent`（`agent.ts:166-167`：`isReadOnly` 为 false、`isConcurrencySafe` 为 true）以及配置描述符造出的同类工具都会进并发阶段。Rust 同样只看 `isConcurrencySafe`。

场景 `concurrent-error-lets-started-siblings-finish` 与 `non-readonly-safe-tool-outlives-concurrent-error-in-ts`（`drain: true`）录下了 TS 行为：`Promise.all` 在 `settledAt` 立即拒绝，已启动的兄弟调用在后台跑完（`settledAt` 之后的 `end` 事件）。Rust 的拒绝时刻与 TS 一致；出错那一轮仍把每个兄弟 poll 一次，然后把没跑完的兄弟交给 tokio 运行时继续跑（不截断 `agent` 这类会写入的调用）。整个 batch 被丢弃（宿主中止、外层 deadline）时仍同步丢弃全部兄弟。

回放对 `drain` 场景断言：Rust 返回时刻等于 TS `settledAt`；随后让虚拟时钟走到兄弟全部结束，全部事件（含 `settledAt` 之后的 `end`）与 TS 相同。`concurrent_error_rejects_immediately_and_lets_started_siblings_run_on` 另用两个永不结束的兄弟钉住「立即拒绝、兄弟不被丢弃」。

此前（到 2026-11-12）Rust 有两处有意偏离：并发组只收 `isConcurrencySafe && isReadOnly` 的工具（A6.5-TOOLS-SCHEDULE-001），出错时丢弃兄弟（A6.5-TOOLS-CANCEL-001）。bbx 场景 `loop-parallel-agent` 显示前者在对外输出上可见（`write_file` 排在 `agent` 前面时，基线的子代理先跑、看不到随后写出的文件），两处一并撤销。

## 深层嵌套数组的 String()

`String(value)`（模板字符串、`in` 的键、`Array#join`）统一用共享实现 `crate::js_value::to_string_units`：迭代实现，超过 `MAX_ARRAY_DEPTH`（2000 层数组）报 V8 的 `RangeError: Maximum call stack size exceeded`，不会让 Rust 栈溢出。V8 的上限随调用栈变化（js_value.rs 记录从 2516 起抛错；本次探针在 2515 层也偶尔抛），所以 2001 到约 2516 层之间 Rust 抛错而 TS 能转换，这是已知偏离，方向是更严。

场景 `deep-array-*-under`（2000 层，两边都能转换）与 `-over`（3000 层，两边都抛错）逐条路径对拍：enum 成员、type 描述、required 键、maxOutputTokens、错误 details 的 exitCode（这五条 TS 都是整批拒绝），以及预览里 file 的 path（TS 在 execute 的 try 内抛错，结果是 `tool_execution_failed` 错误结果）。`matchesType` 的递归在 V8 里比 String() 走得更深（3000 层可以，20000 层抛错），所以 `deep-array-type-match-*` 用 2000 与 20000 两档；Rust 的 type 遍历上限比 String() 多一层，保证外层 `type` 数组加一个正好在上限上的成员仍能走到描述拼接。精确的 Rust 边界（2000 能转换、2001 抛错；type 遍历是 2001 与 2002）由 `string_conversion_depth_bound_is_shared_js_value_limit` 钉住。
