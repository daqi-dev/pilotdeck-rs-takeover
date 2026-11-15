# Native-only feature门控独立追踪

冻结提交：首次`ec88f040cfe3136f300d8327686b20a55aaaad98`；修正候选`0291dbe02651e51e40d8f1689fe6f2e0c2930693`。旧追踪文件保持原SHA，本任务未运行Cargo、未写主ledger/remote。

四项逻辑分别属于生产门控、测试编译前提、fixture错误期待、Clippy死代码门控，不混称四项产品修复。

| 稳定ID | 改动 | 类别 | 首次提交 |
| --- | --- | --- | --- |
| `iface.change.native-only-production-wasm-gates` | Native-only生产宿主隔离可选Wasm依赖 | Kernel生产feature门控 | `ec88f040cfe3136f300d8327686b20a55aaaad98` |
| `iface.change.native-only-wasm-test-module-gates` | Wasm测试模块仅在对应feature下编译 | 测试编译前提门控 | `ec88f040cfe3136f300d8327686b20a55aaaad98` |
| `iface.change.native-only-refusal-fixture-precedence` | Native-only拒绝矩阵纠正错误优先级期待 | 测试fixture修正（非生产逻辑fix） | `ec88f040cfe3136f300d8327686b20a55aaaad98` |
| `iface.change.native-only-approval-helper-deadcode-gates` | 仅Wasm入口使用的审批helper按feature门控 | Clippy死代码编译门控 | `0291dbe02651e51e40d8f1689fe6f2e0c2930693` |

## iface.change.native-only-production-wasm-gates

此前未启用wasm-host仍编译Wasm端口/sidecar bridge和disabled wrapper签名，组合构建触发未链接agentcpm_wasm_host；已有feature合同未被所有引用尊重。 Wasm专属startup模块/session_ports/sidecar安装加cfg；LoadContext ports和loader实参加同门；off wrapper移除Wasm类型参数并使用公开iface HttpTransport，继续明确FeatureDisabled。

- 状态归属：Kernel生产组装、可选依赖与装载器。
- 合同：`iface.contract.assembly`, `iface.contract.packaging`, `iface.contract.tool`；任务：`iface.P2.embedding`, `iface.P2.tools-auth`, `iface.P4.composition`, `iface.P6.release`。
- 冻结源码：`kernel/host/src/startup/mod.rs:188`（`pub(crate) mod sidecar_bridge;`；blob `467b8e525d5c1c6a988c1ab42dd245dc4a641208`）；`kernel/host/src/startup/mod.rs:210`（`pub(crate) mod wasm_kernel_services;`；blob `467b8e525d5c1c6a988c1ab42dd245dc4a641208`）；`kernel/host/src/startup/mod.rs:213`（`pub(crate) mod wasm_ports;`；blob `467b8e525d5c1c6a988c1ab42dd245dc4a641208`）；`kernel/host/src/startup/wasm_factory.rs:373`（`_http: Option<std::sync::Arc<dyn agentcpm_iface::env::HttpTransport>>`；blob `1f3619dbb44c395a191f15670bba528ec27baeb6`）；`kernel/host/src/transport/assembly/prepare.rs:93`（`session_ports: Option<agentcpm_wasm_host::host_services::SessionPorts>`；blob `18411e4b9a37e2ac11f53f985e11d7daf1cc62b7`）；`kernel/host/src/transport/server/assemble.rs:227`（`if !crate::startup::sidecar_bridge::install`；blob `e5b500aaf28078c2204878c1c41775be261cf9aa`）。
- 限制：没有新增Cargo feature或改变默认feature集合；本项尊重既有wasm-host可选边界。
- 限制：off请求不是默认请求：Cargo artifact默认实际[default,wasm-host]，off实际[]，分别保存产物。
- 限制：74表示不同feature配置下的标准测试执行次数，不是74个独立定义；custom1另计。
- 限制：Kernel构建/局部Native真实样例不等于PilotDeck正式入口联合验收；productionWired/jointlyVerified不自动提升。

## iface.change.native-only-wasm-test-module-gates

wasm/wasm_approval_question/wasm_state/wasm_v2测试模块在off时仍引用可选Wasm实现，会污染Native-only编译。 四个专属测试模块随wasm-host feature编译；共享Native/approval测试继续独立运行。

- 状态归属：Kernel测试模块编译边界。
- 合同：`iface.contract.assembly`, `iface.contract.tool`, `iface.contract.auth`, `iface.contract.packaging`；任务：`iface.P2.tools-auth`, `iface.P6.contracts`, `iface.P6.release`。
- 冻结源码：`kernel/host/src/transport/service/tests/mod.rs:679`（`mod wasm;`；blob `3dd9482c83ab45a1e3986bcf692b9d669a940880`）；`kernel/host/src/transport/service/tests/mod.rs:682`（`mod wasm_approval_question;`；blob `3dd9482c83ab45a1e3986bcf692b9d669a940880`）；`kernel/host/src/transport/service/tests/mod.rs:685`（`mod wasm_state;`；blob `3dd9482c83ab45a1e3986bcf692b9d669a940880`）；`kernel/host/src/transport/service/tests/mod.rs:688`（`mod wasm_v2;`；blob `3dd9482c83ab45a1e3986bcf692b9d669a940880`）。
- 限制：off下cfg禁用模块没有执行，不能计为Wasm行为已覆盖。
- 限制：两feature27个共享approval名字一致，表明重复执行同一选择，不代表所有feature特有测试均已运行。
- 限制：74表示不同feature配置下的标准测试执行次数，不是74个独立定义；custom1另计。
- 限制：Kernel构建/局部Native真实样例不等于PilotDeck正式入口联合验收；productionWired/jointlyVerified不自动提升。

## iface.change.native-only-refusal-fixture-precedence

ec88新增矩阵错误期待所有Wasm计划都先返回FeatureDisabled；实际world0.2声明net.http应先被已有世界能力合同拒绝，第4组合断言失败。 0291只修测试期待：6个有效计划因feature关闭拒绝，2个world0.2/net.http无效计划按合同先拒绝；8组合每个保持无半激活发布，之后local正例仍可加载。

- 状态归属：测试错误优先级与装配一致性断言。
- 合同：`iface.contract.atomic-install`, `iface.contract.assembly`, `iface.contract.tool`；任务：`iface.P2.atomic-install`, `iface.P2.embedding`, `iface.P6.contracts`。
- 冻结源码：`kernel/host/src/transport/assembly/tests/plan_prepare.rs:174`（`fn native_only_prepare_refuses_each_wasm_loader_without_partial_publication()`；blob `569fc82974ae2c9d7670cfbe8f4fdf2204b776d2`）；`kernel/host/src/transport/assembly/tests/plan_prepare.rs:205`（`if api == "0.2.0" && !capabilities.is_empty()`；blob `569fc82974ae2c9d7670cfbe8f4fdf2204b776d2`）；`kernel/host/src/transport/assembly/tests/plan_prepare.rs:229`（`let prepared = prepare_components(&local, &context).expect("local loader remains usable")`；blob `569fc82974ae2c9d7670cfbe8f4fdf2204b776d2`）。
- 限制：ec88首次失败只达到前4个内部组合，后4及该测试的local正例未达到；不把源码中的断言当成当时实际执行。
- 限制：8内部组合属于1个测试，不增加8个测试定义/运行计数。
- 限制：74表示不同feature配置下的标准测试执行次数，不是74个独立定义；custom1另计。
- 限制：Kernel构建/局部Native真实样例不等于PilotDeck正式入口联合验收；productionWired/jointlyVerified不自动提升。

## iface.change.native-only-approval-helper-deadcode-gates

ec88 no-default all-targets -D warnings因question解析常量/parse/value_text及guest登记/指纹/撤回helper等7个dead_code失败；它们唯一登记入口来自Wasm。 仅Wasm使用的MAX_PARAMS/MAX_OPTIONS/DeclaredQuestion.parse/value_text、envelope import、payload_digest/register_ask/withdraw随wasm-host编译；共享展示、decide/terminate与授权决定保持可用。

- 状态归属：Kernel审批登记入口与共享审批读面。
- 合同：`iface.contract.auth`, `iface.contract.tool`, `iface.contract.assembly`, `iface.contract.packaging`；任务：`iface.P2.tools-auth`, `iface.P6.contracts`, `iface.P6.release`。
- 冻结源码：`kernel/host/src/transport/service/approval/question.rs:42`（`pub(crate) const MAX_PARAMS`；blob `1a9280c6063af2a3444e88d2dcbea0946fcdf07f`）；`kernel/host/src/transport/service/approval/question.rs:77`（`pub(crate) fn parse(payload:`；blob `1a9280c6063af2a3444e88d2dcbea0946fcdf07f`）；`kernel/host/src/transport/service/guest_approval.rs:45`（`pub(crate) fn payload_digest`；blob `8651dea1a8d1ab86f70a1be39f83496525ec2359`）；`kernel/host/src/transport/service/guest_approval.rs:247`（`pub(crate) fn register_ask`；blob `8651dea1a8d1ab86f70a1be39f83496525ec2359`）；`kernel/host/src/transport/service/guest_approval.rs:395`（`pub(crate) fn withdraw`；blob `8651dea1a8d1ab86f70a1be39f83496525ec2359`）。
- 限制：这是编译/死代码门控，不是授权决策逻辑改写。
- 限制：0291审批27+27通过及两种Clippy通过不能重标ec88的off Clippy首败。
- 限制：74表示不同feature配置下的标准测试执行次数，不是74个独立定义；custom1另计。
- 限制：Kernel构建/局部Native真实样例不等于PilotDeck正式入口联合验收；productionWired/jointlyVerified不自动提升。

## 实际验证与计数

ec88：default/off生产库都构建PASS；off plan8执行7PASS/1FAIL，矩阵第4组合fixture期待错误，后4组合和local正例没有到达；off readonly custom1完整PASS。default plan7、prepare5与default Clippy PASS；off Clippy 7项dead_code FAIL。首败不被0291覆盖。

0291：实际default artifact features为`[default,wasm-host]`，off为`[]`。标准执行74次=off plan8+off approval27+default plan7+default prepare5+default approval27；另有off readonly custom1完整PASS。27个approval名字在两配置一致，这是重复执行而非54个独立定义。fmt及两种features各自Clippy PASS。Wasm cfg禁用模块不计执行覆盖。

生产库公共debug/libagentcpm_host.rlib被后一次off构建覆盖；原default/off独立hashed deps rlib均仍留存并与各自after-build SHA一致。JSON保留首次common-output postcheck的stable=false和后续retained证据，未涂改成公共路径稳定。

Mac只记录实际有证据的组合：PD91a1+Kernelae40构建13条E0433 FAIL，未运行测试，不能重标ec88首败；PD1cce+Kernel0291组合no-run构建PASS，Host artifact features[]。前序失败轮没有成功Host feature artifact，未猜填实际features；成功构建也不等于生产运行或产品联合验收。

| 内部组合 | kind | API | capabilities | 0291期待 | ec88实际到达 |
| --- | --- | --- | --- | --- | --- |
| 1 | wasm | 0.1.0 | [] | WasmHostFeatureDisabled | reached PASS |
| 2 | wasm | 0.1.0 | ['net.http'] | WasmHostFeatureDisabled | reached PASS |
| 3 | wasm | 0.2.0 | [] | WasmHostFeatureDisabled | reached PASS |
| 4 | wasm | 0.2.0 | ['net.http'] | world capability contract rejection | reached fixture assertion FAIL |
| 5 | wasm-host | 0.1.0 | [] | WasmHostFeatureDisabled | NOT_REACHED_AFTER_FIRST_FAILURE |
| 6 | wasm-host | 0.1.0 | ['net.http'] | WasmHostFeatureDisabled | NOT_REACHED_AFTER_FIRST_FAILURE |
| 7 | wasm-host | 0.2.0 | [] | WasmHostFeatureDisabled | NOT_REACHED_AFTER_FIRST_FAILURE |
| 8 | wasm-host | 0.2.0 | ['net.http'] | world capability contract rejection | NOT_REACHED_AFTER_FIRST_FAILURE |

8组合是1个测试内部循环，0291完整运行包含最后local正例；不是额外8个测试。

| 实际选择 | 结果/类别 | 实际feature artifact |
| --- | --- | --- |
| `ec88f040-first-host-production-default` | PASS / build | `[["default", "wasm-host"]]` |
| `ec88f040-first-host-production-native-only` | PASS / build | `[[]]` |
| `ec88f040-first-native-only-assembly-plan` | FAIL / executed | `[[]]` |
| `ec88f040-first-native-only-readonly-tools` | PASS / executed | `[[]]` |
| `ec88f040-first-default-assembly-plan` | PASS / executed | `[["default", "wasm-host"]]` |
| `ec88f040-first-default-assembly-prepare` | PASS / executed | `[["default", "wasm-host"]]` |
| `ec88f040-first-fmt` | PASS / build | `[]` |
| `ec88f040-first-clippy-native-only` | FAIL / build | `[]` |
| `ec88f040-first-clippy-default` | PASS / build | `[]` |
| `0291dbe0-first-host-production-default` | PASS / build | `[["default", "wasm-host"]]` |
| `0291dbe0-first-host-production-native-only` | PASS / build | `[[]]` |
| `0291dbe0-first-native-only-assembly-plan` | PASS / executed | `[[]]` |
| `0291dbe0-first-native-only-approval` | PASS / executed | `[[]]` |
| `0291dbe0-first-native-only-readonly-tools` | PASS / executed | `[[]]` |
| `0291dbe0-first-default-assembly-plan` | PASS / executed | `[["default", "wasm-host"]]` |
| `0291dbe0-first-default-assembly-prepare` | PASS / executed | `[["default", "wasm-host"]]` |
| `0291dbe0-first-default-approval` | PASS / executed | `[["default", "wasm-host"]]` |
| `0291dbe0-first-fmt` | PASS / build | `[]` |
| `0291dbe0-first-clippy-native-only` | PASS / build | `[]` |
| `0291dbe0-first-clippy-default` | PASS / build | `[]` |
| `mac-pd-91a1cbe3-combined-build-v1` | FAIL / build | `[]` |
| `mac-pd-1cceb75e-kernel0291-combined-build-v2` | PASS / build | `[{"package_id": "path+file:///Users/da/.codex/worktrees/pilotdeck-iface-integration/pilotdeck-rs-cc2c/deps/agent-kernel/kernel/host#agentcpm-host@0.1.6", "features": [], "manifest": "/Users/da/.codex/worktrees/pilotdeck-iface-integration/pilotdeck-rs-cc2c/deps/agent-kernel/kernel/host/Cargo.toml"}]` |

每条完整argv、UTC、commit、feature metadata、产物/测试二进制SHA、receipt/log依赖均在JSON。proposedRunId只供主ledger唯一写入者核实关联。productionWired与jointlyVerified保持UNKNOWN，未把Kernel局部Native样例当成PilotDeck正式组装验收。
