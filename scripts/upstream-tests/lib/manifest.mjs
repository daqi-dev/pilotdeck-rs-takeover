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
};

export const EXCLUSIONS = [
  // 由 run.mjs 的实测结果填写，见 README「剔除的用例」。
];

export function exclusionFor(group, name) {
  return EXCLUSIONS.find((item) => item.group === group && item.name === name);
}
