# Compat shim 脚本合同 fixture（FIXTURE 线）

本目录是 27 个 `cli.case.compat.*` 入口的**脚本合同机器可读正本**：每条
`cases/<slug>.json`（slug 与 `src/mappings/` 一致）描述若干 scenario，由
`tests/contract_inproc.rs`（A 层：进程内 pub API + FakeExecutor）与
`tests/contract_blackbox.rs`（B 层：spawn 真实 `pilotdeck` 二进制）两个
runner 消费。fixture 本身不区分层，由 scenario 字段声明在哪一层执行。

期望值溯源：映射数据（`src/mappings/<slug>.json`，正本
`_tmp/20261118-s4/mappings-draft.json`）+ 各 case 验收点 + 已裁决行为。
标注 `"pendingS3": true` 的 scenario 只断言宽松口径（exit code + 错误码
出现），S3 交付对应操作后由集成者切真实联调（S4 计划 §6.3）。

## 顶层 schema

```json
{
  "slug": "sdk-run",                              // 与 mappings/ 一致
  "caseId": "cli.case.compat.cli.compat.sdk-run", // 台账 case id
  "scenarios": [ ... ]                             // 至少 1 条
}
```

## Scenario 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `name` | string | 场景名（slug 内唯一） |
| `summary` | string | 一句话：该场景钉住什么合同点 |
| `inproc` | bool \| "guarded" | A 层是否执行（默认 true）。`"guarded"` = 仅当测试进程环境未设 `AGENTCPM_PD_HOME` 时执行（A 层无法安全清除进程级 env；真实环境不设该变量） |
| `blackbox` | "local" \| "fake" \| "signal" \| "none" | B 层执行方式（默认 "local"）。`local`：无需 fake 目标（PATH 指向空目录，任何业务调用都会以 target-not-found 失败，从而反向证明"无业务调用"）；`fake`：需要 `fixtures/fake_agentcpm_pd.py`（python3）；`signal`：B 层信号场景（unix，需 python3）；`none`：B 层不执行 |
| `argv` | string[] | 不含程序名的完整 argv |
| `stdin` | string \| null | 写入子进程 stdin 的内容；null → 关闭 stdin（B 层） |
| `binName` | string | 以该名调用二进制（多入口名：`pilotdeck-agent-loop-sidecar` / `pilotdeck-gateway`；默认 `pilotdeck`）。A 层用它驱动 `reject::bin_name_rejection` 路由断言 |
| `homeEnv` | bool | B 层是否设 `AGENTCPM_PD_HOME` 指向隔离临时 home（默认 true；false = 显式清除该变量） |
| `legacyEnv` | object | 旧环境变量（如 `PILOTDECK_AGENT_LOOP_FACTORY`）；B 层先清除全部已知旧变量再设置这些 |
| `shimConfig` | object | 写入 `<home>/compat/config.json` 的完整 v2 文档（schema 与 `src/config.rs` 一致，camelCase） |
| `shimSessions` | object | 写入 `<home>/compat/sessions.json` 的旧→新会话映射：`{"<legacy-id>": {"sessionId": "...", "projectId": "..."}}` |
| `fakeScript` | array | fake 响应序列，元素 `{"method": "...", "respond": {...}}`，**必须按实际调用顺序排列**（A 层按序消费 FakeExecutor 队列；B 层 fake 按 method 匹配消费，两层一致要求顺序列出） |
| `fakeDefault` | object | 可选：队列耗尽时对该 method 的兜底响应（信号场景里维持 run.events 轮询） |
| `pendingS3` | bool | 期望为宽松口径，待 S3 联调收紧（默认 false） |
| `note` | string | 溯源说明（映射字段/行为裁决出处） |

### respond 形态

- 成功/受理：`{"outcome": "succeeded"|"accepted", "result": {...}}`
- 合同失败：`{"outcome": "failed", "error": {"code": "...", "message": "..."}}`
  ——错误码透传断言（`CAPABILITY_UNAVAILABLE` 等）用这种。
- 响应丢失（shim 侧视为可重试的传输失败）：
  `{"drop": true, "exit": 2, "stderr": "..."}` —— 子进程无 envelope 退出。

## expect 字段

| 字段 | 层 | 说明 |
|---|---|---|
| `exitCode` | A+B | 进程退出码 / `run()` 返回值 |
| `exitSignal` | B | 期望被信号杀死（目前仅 `"SIGINT"`；shell 报告的 130 = 128+2，进程侧断言信号本身） |
| `rejectCode` | A | `binName` 路由断言：`bin_name_rejection` 返回的 Rejection 的错误码 |
| `invokeMethods` | A+B | 精确调用方法序列。A 层 = FakeExecutor 记录；B 层 = fake 的 calls.jsonl，允许尾部附加 guardian 方法（`requests.lookup`/`run.cancel`/`run.get`）——前缀必须精确匹配 |
| `invokeMethodsContains` | B | 信号场景：log 中必须出现的 method（顺序无关） |
| `calls` | A+B | 选中调用的逐条断言（游标定位：每条匹配其 method 在上一次匹配之后**首次出现**的那次调用，中间可穿插其他调用；完整序列由 `invokeMethods` 负责）：`{"method": "...", "input": {...}, "requestKey": true\|false, "inputNotContains": ["port"]}`。`input` 为**子集匹配**（期望对象的每个键在实际 input 中递归相等；数组要求长度相等且逐元素子集）；`requestKey` 断言该次调用是否携带持久幂等键 |
| `executorTarget` | A | FakeExecutor 上 `set_target` 后的 target 档案名 |
| `requestKeyReuse` | A | `{"method": "...", "count": N}`：该方法的调用恰好 N 次且全部复用同一个幂等键 |
| `sameFieldAcrossCalls` | A | `{"field": "sessionId", "indices": [0, 1]}`：这些序号调用的 input 中该字段值相同（跨调用一致性，如同一会话） |
| `callsInputExcludes` | A | 字符串数组：任何记录到的调用 input 序列化后不得包含这些子串（如 auth token） |
| `stdoutJson` | B | stdout 解析为 JSON 文档序列（先整体解析；失败则按行 JSONL），每元素与期望**子集匹配** |
| `stdoutContains` / `stdoutExcludes` | B | stdout 子串包含 / 排除（secret 不出现类断言） |
| `stdoutEmpty` | B | stdout 为空 |
| `stderrContains` / `stderrExcludes` | B | stderr 子串包含 / 排除 |
| `homeAbsent` | B | 相对 home 的路径数组：断言不存在（"参数失败不产生业务写入"的落盘面） |
| `runRecord` | B | 信号场景：`<home>/compat/runs/*.json`（恰一个）中的 `state` 与 `terminalState` 期望值 |

## 覆盖统计与口径

见各 fixture 的 `summary` 字段与场景 `note`；总覆盖矩阵在 FIXTURE 线的
阶段报告里（每 case：主断言场景数、关键断言点、"待 S3 联调"项）。

## fake agentcpm-pd

`fake_agentcpm_pd.py` 由测试在运行前复制到临时目录命名为 `agentcpm-pd`
并 chmod 755，仅通过**子进程 env 的 PATH 前缀**注入（绝不修改测试进程
自身的 PATH）。python3 不可用时 fake/signal 场景整体 skip（对应测试函数
带 `#[ignore]` + 运行时探测；真实门禁 cb8/mbp2 有 python3）。
