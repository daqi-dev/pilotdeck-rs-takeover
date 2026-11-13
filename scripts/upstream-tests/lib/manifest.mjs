// 上游用例的剔除清单：哪些用例不进验收集、为什么。
// 只能整例剔除，不改、不删任何一条断言。每条都要写清理由；理由的证据（次数、输出）记在 README。
//
// kind：
//   inprocess  用例读的是进程内 sidecar 的内部状态（spy、自定义执行函数、注入的确定性 id），
//              换成外部进程后这条断言在原理上不能成立。
//   unstable   指向基线自己的 sidecar（外部进程）连跑时时红时绿，用例本身不可靠。
//   external   需要真实外部服务。
//
// 「不经过 sidecar 的宿主自测」不在这里登记：run.mjs 在运行时看用例有没有碰到 sidecar，
// 没碰到的单列为「不计入」。

export const GROUPS = {
  client: {
    source: 'dist/tests/agent/modules/sidecar-client.spec.js',
    about: 'sidecar 客户端（TS 宿主）+ 外部 sidecar：loopbackConnection 与直接拉起编译产物的用例',
  },
  tcp: {
    source: 'dist/tests/agent/modules/tcp-sidecar-transport.spec.js',
    about: 'TCP 部署：直接拉起编译产物监听本机 TCP 的用例',
  },
  deployment: {
    source: 'dist/tests/cli/local-gateway-agent-loop-deployment.spec.js',
    about: 'createLocalGateway 选中 stdio / TCP 部署的端到端用例',
  },
  stdio: {
    source: 'dist/tests/agent/modules/sidecar.spec.js',
    about: 'sidecar server 的 NDJSON 进出：用例直接往 stdin 写帧、从 stdout 读帧，改成驱动外部子进程',
  },
};

// 经过被测 sidecar 的用例（计入候选）。由基线外部进程的一次全绿运行里「碰到 sidecar」的记录得出，
// 之后每次运行都核对：通过的用例碰没碰 sidecar 必须与这里一致，否则报「分类不符」。
// 不在这里的用例要么不经过 sidecar（宿主自测、或进程内 TS sidecar server 配自定义执行函数），要么在 EXCLUSIONS。
export const SIDECAR_TESTS = {
  client: [
    "capability-only sidecar factory completes a durable host tool turn and restores terminal seed state",
    "production sidecar receives the first model delta before the provider finishes",
    "production sidecar closes the host provider iterator when its consumer stops early",
    "production sidecar budget module stops before host tool side effects and persists one status",
    "production sidecar does not acknowledge a status event when durable persistence fails",
    "production sidecar turn module drains and acknowledges live steer exactly once",
    "production sidecar persists compaction before the compacted model request",
    "production sidecar advertises elicitation availability without serializing the channel",
    "sidecar connection factory receives only transport turn facts",
    "sidecar execute wire preserves AgentLoop turn limits and model configuration",
    "sidecar known terminal keeps host tool checkpoint state and current attachment authorization",
    "sidecar permission callback uses the canonical host tool cwd",
    "sidecar host owns tool-driven plan mode across callbacks and turns",
    "sidecar host applies submit ask mode before capability execution",
    "sidecar factory projects a preflight final execute rejection as one failed turn",
    "stdio sidecar connection runs the built binary and keeps model and tool execution in the host",
    "stdio sidecar keeps one unknown terminal when a host tool returns after its deadline",
    "sidecar agent tool delegates through the host-owned one-shot subagent port",
    "loopback sidecar flushes AgentLoop-emitted events to the host before final",
    "sidecar binds the auxiliary model before executing web_fetch in llm mode",
    "sidecar runner binds host routing before every submitted turn",
    "production sidecar refreshes a host-owned deferred tool catalog before the next model request",
    "production sidecar preserves host prepare request materialization through execution",
    "prepared output caps constrain normal native and sidecar requests before provider limits",
    "production sidecar applies a steer attachment to host tools only after the steer is durable",
    "production sidecar carries persistent model output caps into the next child turn",
    "production sidecar emits host tool progress before a slow tool completes",
    "production sidecar keeps the host file checkpoint when tool-result durability fails",
  ],
  tcp: [
    "built sidecar CLI serves Module Protocol over local TCP when configured",
    "built TCP sidecar restart before_effect persists host and operation evidence without replay",
    "built TCP sidecar restart after_effect persists host and operation evidence without replay",
  ],
  deployment: [
    "local Gateway abort cancels a stdio sidecar agent-tool child through the host one-shot subagent port",
  ],
  // sidecar.spec 的用例都自带 factory；换成外部进程后跑的是生产的默认 factory（createSidecarExecution），
  // 用例写进 stdin 的帧和对 stdout 的断言不变。只计入「指向基线外部进程 20 次全过」的这几例：
  // factory 不会被调用（capabilities / 截止已过的 execute），或者断言只看协议层的回应与终态。
  stdio: [
    "sidecar capabilities do not advertise unsupported stream resume",
    "sidecar status returns the live snapshot of an accepted execution",
    "default sidecar server preserves a host model preparation through AgentLoop request normalization",
    "sidecar abort releases a pending module call",
    "sidecar classifies a matching host timeout cancel as result_unknown",
    "sidecar rejects an execute whose deadline expires before it starts",
  ],
};

const STDIO_CUSTOM_FACTORY = '用例自带的 factory 决定了执行的结果（固定的 loop 结果、受控的门、伪造的模块调用），'
  + '外部进程里只能是生产的默认 factory，断言针对的行为在外部进程上不存在；指向基线外部进程 20 次全红';

export const EXCLUSIONS = [
  { group: 'stdio', kind: 'inprocess', name: 'sidecar assigns a distinct default module instance id to each process instance',
    reason: '断言读的是进程内 server 对象的 moduleInstanceId 属性，并注入了确定性 uuid' },
  { group: 'stdio', kind: 'inprocess', name: 'resumable sidecar rebinds a live stream without replaying an applied event',
    reason: `给 server 传了自定义 capabilities（开启 resume）与确定性 uuid；${STDIO_CUSTOM_FACTORY}` },
  { group: 'stdio', kind: 'inprocess', name: 'sidecar resume replays a pending module call and fences a stale host response',
    reason: `给 server 传了自定义 capabilities（开启 resume）与确定性 uuid；${STDIO_CUSTOM_FACTORY}` },
  { group: 'stdio', kind: 'inprocess', name: 'sidecar server round-trips host module calls and emits one terminal event',
    reason: `给 server 传了自定义 moduleId；${STDIO_CUSTOM_FACTORY}` },
  { group: 'stdio', kind: 'inprocess', name: 'sidecar retry keeps operation identity and clears a recovered module failure',
    reason: `factory 内计数（spy）；${STDIO_CUSTOM_FACTORY}` },
  { group: 'stdio', kind: 'inprocess', name: 'sidecar terminal preserves the AgentLoop error instead of replacing it with a host failure', reason: STDIO_CUSTOM_FACTORY },
  { group: 'stdio', kind: 'inprocess', name: 'sidecar emits result_unknown when a capability acknowledgement is fenced', reason: STDIO_CUSTOM_FACTORY },
  { group: 'stdio', kind: 'inprocess', name: 'sidecar does not continue after a host capability result_unknown response', reason: STDIO_CUSTOM_FACTORY },
  { group: 'stdio', kind: 'inprocess', name: 'sidecar preserves a terminal host capability failure without another model turn', reason: STDIO_CUSTOM_FACTORY },
  { group: 'stdio', kind: 'inprocess', name: 'sidecar fences stale cancel and duplicate execute against the active operation identity',
    reason: `断言 factoryCalls === 1（进程内 spy）；${STDIO_CUSTOM_FACTORY}` },
  { group: 'stdio', kind: 'inprocess', name: 'sidecar maps max_turns to a failed protocol outcome with the original result', reason: STDIO_CUSTOM_FACTORY },
  { group: 'stdio', kind: 'inprocess', name: 'sidecar supplies the canonical max_turns code when the result has no error', reason: STDIO_CUSTOM_FACTORY },
  { group: 'stdio', kind: 'inprocess', name: 'sidecar resolves an active deadline as result_unknown for the host operation owner', reason: STDIO_CUSTOM_FACTORY },
];

// 对拍工具（tools/agent-loop-parity，53 个 PilotDeck 场景）里不计入的场景：
// 基线自比（TS 原生循环 vs 基线 sidecar）不能每轮都过的，见 README「parity」。
// 2026-11-13 macOS 上 6 轮基线自比：17 个场景每轮都过，下列 36 个不是（26 个 6 轮全红，10 个时红时绿）。
// 失败几乎都是 trace 里跨通道事件的先后（agent.status、context.budget 相对 model.request 的位置），
// 取决于 sidecar 进程与宿主之间的时序，而不是语义。
export const PARITY_EXCLUSIONS = [
  { scenarioId: "single_tool", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "multiple_tool", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "tool_error", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "permission_denial", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "checkpoint_resume", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 2 轮' },
  { scenarioId: "permission_allow", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "permission_ask_approve", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "permission_ask_deny", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "multi_tool_ordered", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "multi_tool_mixed_permission", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 2 轮' },
  { scenarioId: "multi_tool_mixed_error", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "large_tool_result", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "tool_retryable_error", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "tool_non_retryable_error", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "model_retryable_error", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 5 轮' },
  { scenarioId: "malformed_model_response", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 5 轮' },
  { scenarioId: "stream_interruption", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 5 轮' },
  { scenarioId: "multimodal_image_and_text", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 4 轮' },
  { scenarioId: "allowed_read_files", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "denied_read_files", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "write_snapshot_resume", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 3 轮' },
  { scenarioId: "plan_mode_host_policy", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 1 轮' },
  { scenarioId: "plan_mode_bypass_host_policy", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "sidecar_budget_limit", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "sidecar_elicitation_execution", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "sidecar_sdk_tool_progress", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "sidecar_live_steer", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "sidecar_durable_compaction", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "sidecar_full_request_compaction_budget", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "sidecar_projected_request_compaction_budget", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "sidecar_seed_read_state", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "sidecar_live_model_stream", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 1 轮' },
  { scenarioId: "sidecar_model_metadata", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 5 轮' },
  { scenarioId: "sidecar_parent_close_after_admission", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "sidecar_one_shot_subagent_success", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
  { scenarioId: "sidecar_one_shot_subagent_failure", reason: '基线自比（TS 原生 vs 基线 sidecar）6 轮只过 0 轮' },
];

// 基线 sidecar 自己发出的、违反基线 schema 的帧（疑似基线 bug，Rust 不照做）。
// 只在 --target baseline 时豁免；指向 Rust 时同样的违规照常判失败。
export const KNOWN_BASELINE_SCHEMA_VIOLATIONS = [
  // src/agent/modules/llm/hostModelInvokerPort.ts:101 把 AbortSignal 放进 call，
  // agentLoopSidecarServer.ts callModule 原样展开进 module_call 帧，序列化成 "abortSignal":{}。
  'moduleCallRequest: / additionalProperties {"additionalProperty":"abortSignal"}',
];

export function exclusionFor(group, name) {
  return EXCLUSIONS.find((item) => item.group === group && item.name === name);
}
