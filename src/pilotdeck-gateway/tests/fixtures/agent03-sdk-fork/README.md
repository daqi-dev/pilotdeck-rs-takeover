# AGENT03 SDK fork 正式场景候选

状态：**待固定 TS 校准，尚不可计分**。真实服务入口与 SDK `submit_turn.sdkSessionConfig`，本地模型模拟按消息阶段发 `agent`、`read_skill` 和 child-only MCP 调用。SDK parent 允许 alpha/beta；worker 只选 alpha、禁用 memory、独立 initialPrompt、独立 stdio MCP，并指定命名 observer；observer 定义故意要求写工具、MCP、beta 和 initialPrompt，以检查基线强制只读/清理。另开一会话以检查 parent/child 会话作用域。

运行时仅使用隔离 HOME/PILOT_HOME 和本地 mock，先固定同机同路径两次 TS 自比，然后原始产物运行 `verify.mjs ARTIFACT scenario.json`，再固定 Rust 源码与 binary 运行同一场景。BBX 本机即使 slot 非零也检查固定 18789；该端口被占用时必须用 Docker 私有网络，不能跳过守卫、关闭其他进程或映射宿主端口。首次本机尝试在启动前被守卫拒绝：`_tmp/20261113-agent03-sdk-fork-ts-self-1/runner.log`。

验证器候选正判要求实际 provider 请求的父子/observer 分离、独立有序 initialPrompt、alpha 真实工具结果与 beta 不可读、仅 child MCP 暴露和真实 tools/call 结果、MCP 自己记录 start/initialize/list/call/exit、observer 无写/skill/MCP工具及无自身 initialPrompt、真实 observer_report 和第二 session 原技能仍可见。不可用 provider 规则命中代替 MCP 实际 call，或用 observer 字符串代替 observer 模型请求。UUID、路径、耗时不可通过宽泛规则忽略业务内容。

当前显式缺口：本场景尚无可见的父 memory 正控，`memory: disabled` 的真实效果**尚不可验收**；需给父会话配置可控 memory 内容并先证明父可见，再在 child 和 observer 请求中断言同一标记缺失。还要单独覆盖 SDK `mcpServers` 数组引用、父定义同名隔离以及 MCP 初始化/调用失败和中断时回收。现有 `verify.mjs` 仅为候选合同，真实 TS 首轮可能暴露规则、请求形状或事件投递顺序的假设，必须保存首错并根据实际来源修复；不能只靠修改检查器把它变绿。

新增场景或 checker 必须在实测成功后做真实故障负控（例如将 MCP 响应数据、技能文件或 observer 限制通过隔离 mock 变异），确认严格 verifier 非零并记录首错；修改产物 JSON 的自检只说明 verifier 能识别结果变化，不代替真实故障注入。最终须记录 source/dist、driver、scenario、binary、平台与所有日志身份。该夹具不证明整个 AGENT03 或 SDK 交付。
