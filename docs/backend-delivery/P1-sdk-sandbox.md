# P1 SDK tool_policy sandbox数据配置

日期2026-11-13；负责人h1_inventory（纯eligibility及测试），Email（ExecutionWorld/registry及SDK gate）、O1（SDK snapshot正式装配）、主协调（模块声明及集中验证）。本包不重写OS沙箱，不更改profile.sandbox_mode。

## 固定TS合同

`src/cli/ProjectSessionRuntimeBundle.ts:807-825`实际函数blockedToolNames规则如下。类型/字段语法由InProcessGateway SDK配置验证负责，helper不引入额外默认值或禁用能力。

| 策略 | 实际阻止 |
|---|---|
| process=deny | hostBridge |
| filesystem=deny | kind filesystem或hostBridge |
| filesystem=read_only | 名称write_file/edit_file/edit_notebook或hostBridge |
| network=deny | kind network或hostBridge |

hostBridge精确定义：kind shell/mcp/custom，或name execute_code/agent/以task_开头。不是所有subagent相关名称都自动禁止；bare subagent/send_message若无上述kind，固定函数不会仅因名字阻止。read_only也不是所有filesystem kind一律禁止：未知filesystem工具名必须保留基线规则。大小写、前缀、UTF16字面比较保持；aliases通过canonical tool注销时一并剥除，不在helper猜工具名。

管理策略、organization和explicit allowed/disallowed先形成阻止集合；sandbox只额外阻止，不恢复其它策略拒绝的能力。TS该filter用于初建registry、动态MCP可用工具、子代理registry配置、继续会话等多入口（:340,:361,:616,:691），不是只过滤第一次builtins。

## 产品接口与独占

新`native_process/sdk_sandbox.rs::tool_allowed(sandbox:Option<&JsValue>,name:&JsString,kind:Option<&JsString>)->bool`是无状态纯predicate。输入sandbox为限制对象，不是整个SDK config。Email在完整registry注册builtins/custom/MCP/userdialog/agent/continuations后、以及动态工具刷新后调用；拒绝时实际unregister canonical entry和aliases。admission碰撞检查仍保留完整catalog，不能因工具被隐藏而允许注册重名能力。

Email `tools.rs`唯一owner持ExecutionWorld.sdk_sandbox克隆继承和with_sdk_sandbox；O1接session SDK snapshot；`sdk_config.rs`只有真正接通这些filter后才可加入WIRED_FIELDS。主协调在native_process.rs加mod sdk_sandbox，新测试由helper文件cfgpath挂载；本作者不改公共根模块或tools.rs。

## 测试与证据边界

纯oracle驱动位于`tests/fixtures/sdk-sandbox/oracle.mjs`。从固定TS AST提取原始blockedToolNames+matchesToolSelector，仅在内存转译到隔离VM执行，记录整个原TS源码/dist逐字节校验及两个函数独立hash。没有手写一份TS算法，没有复制源码落盘，没有把纯函数oracle描述成RuntimeBundle/Gateway正式入口。矩阵为12 policies×19 names×10 kinds=2280格，另2格记录与managed/explicit并集的host接线参考。

Rust测试新增2项pure helper及2项真实native调用测试。后者复用既有ControlProvider，仅mock模型HTTP，实际LocalGateway默认native、NativeExtensions注册custom kind工具、真实执行counter：模型强制调用被隐藏能力时counter必须保持0；同gateway另一个无sandbox会话确实执行1；同restricted会话下一turn仍拒绝；正常shutdown后同HOME新gateway重建再带同policy仍拒绝，新无sandbox会话仍可执行。除执行拒绝，原始模型请求tools schema也必须正确排除/包含。没有改OS环境限制来制造拒绝，普通permissionMode bypass也不能绕过SDK能力filter。

这些native测试尚未编译/运行；dynamic MCP刷新、延后工具揭示、aliases和subagent registry须由对应接线owner补实际路径验证，不能用本矩阵代替。没有通过证据前不标交付。

## 当前状态

helper/测试源码已落、直接rustfmt通过。尚待短槽采集固定TS oracle文件、集中all-targets check/4专项，以及Email动态registry回归。未自行Cargo、未运行native产品、未提交、未使用真实账号/模型。


## 固定TS取证完成

已按主分配短槽2执行a/b两轮，2280格+2union，字节相同SHA256 5f41ce84c5b0c268517df2be65d3d3f9f0fe1a1642c2b21c111239e34dea2887。原始 `_tmp/20261113-sdk-sandbox-oracle-{a,b}`；include oracle.json及metadata.json已归档，不再缺文件。四个真实TS原函数的内存fault-injection负控全KILLED：去掉custom hostBridge、去掉task_前缀、去掉network kind、把read_only扩成整类filesystem拒绝；各有实际name/kind/policy首错格与差异分母，原函数源码及oracle磁盘未变，原始 `_tmp/20261113-sdk-sandbox-negative-a/results.json` SHA2e15a089fd50aabf978ec7ca4bc169ccc00ab42b0a69c548e68497eb29a985bb。这是policy装置负控，不是Rust native测试通过。

helper已向Email明确ready；其正式filter链确认后可开放WIRED_FIELDS sandbox，由集中check/4专项和动态registry回归确认，当前仍未Cargo、未执行native专项。模块声明由root处理。
