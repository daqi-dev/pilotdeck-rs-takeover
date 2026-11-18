# 完整模型输入检查独立追踪：0f081de1与ae40

本文件及`state/iface-kernel-input-inspection-ae40.json`为独立下一批，不修改历史36项追踪文件。主ledger已有38项，其中583285a6的独立修正不重复登记。首次产品提交`0f081de151c9b3f4c7c99b21740af4651364af28`，候选`ae40c3234e16ca3d0f0e9a0bcc99f3092ddf2817`；ae40仅两测试文件六处tuple pattern修正，无生产/API变化。

本批8项新逻辑有冻结行号、内容SHA256及Git blob OID，映射13合同/26任务中的相关项。Kernel局部受影响范围通过，PilotDeck正式组装和联合验收尚未完成。

| 稳定ID | 逻辑改动 | 类别 | 首次提交 |
| --- | --- | --- | --- |
| `iface.change.public-complete-input-inspection` | 公开完整输入检查结果与默认Unsupported | 公开ifACE API变更 | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `iface.change.sealed-input-ledger-inspection-binding` | 以封组账本绑定检查输入且缺metadata失败关闭 | Kernel内部修复 | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `iface.change.request-profile-actual-media-gate` | 完整request profile按实际内容决定媒体能力门 | Kernel内部行为修复 | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `iface.change.inspection-outside-lock-double-authorize` | 锁外检查前后复核授权再claim派发 | Kernel生产路径及内部修复 | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `iface.change.input-inspection-queuefull-shape` | QueueFull替身窄吸收正式错误形状 | 上游窄吸收与测试替身修正 | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `iface.change.input-inspection-lc-types-sync` | L-C类型声明窄补两条既有方法名 | 上游窄吸收与SDK类型登记 | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `iface.change.input-inspection-model-identity-registration` | 完整输入检查登记model身份0.1.7 | 兼容版本登记 | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `iface.change.input-inspection-failed-job-pattern-fixture` | 六处Failed tuple模式修复解除首编阻塞 | 测试前提修正（非产品fix） | `ae40c3234e16ca3d0f0e9a0bcc99f3092ddf2817` |

## iface.change.public-complete-input-inspection

已协商rich request缓冲不等于完整schema已验证；不能靠media_input=true或旧commit绕过真实输入检查。 新增ModelInputRequirements(input_id/config_revision/requires_media_input)和inspect_input默认Unsupported；合同要求sealed完整拼接、纯只读无I/O/消费/事件/生成，scoped commit使用同一已检查快照。

- 状态归属：Kernel持有准入；可信profile适配器负责完整解析和媒体分类。
- 合同：`iface.contract.model`, `iface.contract.context`, `iface.contract.auth`；任务：`iface.P2.model-context`, `iface.P3.model`, `iface.P4.loop`, `iface.P6.contracts`。
- 冻结源码：`kernel/iface/src/model_session/input.rs:125`（`pub struct ModelInputRequirements`；blob `e3274e01902d7f18ad1104a24dc45b534c2d2c7e`）；`kernel/iface/src/model_session/operations.rs:170`（`fn inspect_input(`；blob `9ba9839dae4872efd92394cad3feb38d37381494`）；`kernel/docs/notes/model-json-profiles-v1.md:62`（`新增 `ModelSession::inspect_input`；blob `34a99731fd74f1e45e0a51f0abc0793e3e86b82e`）。
- 限制：Rust类型/默认方法不实现产品schema验证；外部legacy negative control只证默认立即拒绝。
- 限制：9个Host input_profile测试使用可信Assessment替身核对门控，不等于真实产品解析器完成所有嵌套媒体分类。
- 限制：API lightweight收据实测HEAD5832 dirty WIP；仅可另作scope源码匹配，不伪造冻结0f/ae40已执行。
- 限制：生产组装与真实PilotDeck联合验收仍未完成；源码和Kernel局部PASS不关闭productionWired/jointlyVerified。

## iface.change.sealed-input-ledger-inspection-binding

未绑定sealed input/revision/job或缺失metadata时若退回旧路径，会派发未检查的请求。 InputLedger新增group_format；prepare读取精确principal/input格式并要求Request模式、协商profile、commit_has_job、config/job/input一致；metadata缺失直接错误，inspection结果ID/revision不符也拒。

- 状态归属：Kernel输入账本、封组与共同模型job。
- 合同：`iface.contract.model`, `iface.contract.state-ownership`, `iface.contract.context`；任务：`iface.P2.model-context`, `iface.P2.history-lifecycle`, `iface.P4.state`, `iface.P6.contracts`。
- 冻结源码：`kernel/model-session/src/input_ledger/commit.rs:80`（`pub fn group_format(`；blob `c3ec9b172a14b07c2fc7c774ec1a68f4fbe4c2c7`）；`kernel/host/src/transport/model_session/execution/input_profile.rs:13`（`pub(crate) fn prepare(`；blob `c4e20b70540246944f82ac326528f542c65b92be`）；`kernel/host/src/transport/model_session/execution/input_profile.rs:76`（`requirements.input_id != commit.input_id`；blob `c4e20b70540246944f82ac326528f542c65b92be`）。
- 限制：group_format本身只读metadata；commit_has_job等组合检查才证明该已准入封组绑定，不把格式读取单独当作seal动作。
- 限制：API lightweight收据实测HEAD5832 dirty WIP；仅可另作scope源码匹配，不伪造冻结0f/ae40已执行。
- 限制：生产组装与真实PilotDeck联合验收仍未完成；源码和Kernel局部PASS不关闭productionWired/jointlyVerified。

## iface.change.request-profile-actual-media-gate

原富请求只要media_input=false即连纯文字也拒绝；分段检查不能发现最后chunk或嵌套工具结果媒体，保留MIME却换input kind也可逃门。 允许无媒体能力的纯文本profile先有界缓冲；保留精确MIME/version/协商检查，reserved MIME必须RequestSnapshot且Request模式；inspect返回实际requires_media_input后由Host决定能力准入，true能力同样必须检查。

- 状态归属：Kernel能力门；适配器实际schema/媒体分类。
- 合同：`iface.contract.model`, `iface.contract.context`, `iface.contract.auth`；任务：`iface.P2.model-context`, `iface.P3.model`, `iface.P4.loop`, `iface.P6.contracts`。
- 冻结源码：`kernel/host/src/transport/model_session/handlers_input/append.rs:220`（`model request profile requires request_snapshot input kind`；blob `4ef575b258e6fa9005b2a9abefe71e991e5c6194`）；`kernel/host/src/transport/model_session/handlers_input/append.rs:253`（`complete model request profile requires request mode`；blob `4ef575b258e6fa9005b2a9abefe71e991e5c6194`）；`kernel/host/src/transport/model_session/execution/input_profile.rs:81`（`requirements.requires_media_input && !inspection.media_allowed`；blob `c4e20b70540246944f82ac326528f542c65b92be`）。
- 限制：取代41e2/c6“纯文本也需media_input=true”的候选限制；旧历史记录不覆盖。
- 限制：duplex不支持此完整请求profile；未知schema/BoundaryOnly不得当完整检查成功，不代表全历史请求/多模态兼容已验收。
- 限制：API lightweight收据实测HEAD5832 dirty WIP；仅可另作scope源码匹配，不伪造冻结0f/ae40已执行。
- 限制：生产组装与真实PilotDeck联合验收仍未完成；源码和Kernel局部PASS不关闭productionWired/jointlyVerified。

## iface.change.inspection-outside-lock-double-authorize

检查future可能等待、超时或遇撤权/config变化；仅检查前授权不能阻止检查完成后的失效派发。 正式worker在初次authorize后锁外inspect，之后走egress/claim中的再次授权/版本复核再scoped commit；continuation同样先authorize、inspect后再authorize。失败为已知派发前拒绝，不调用生成，保留实际准入请求成本。

- 状态归属：Kernel正式worker、授权Permit与claim顺序。
- 合同：`iface.contract.model`, `iface.contract.auth`, `iface.contract.lifecycle`, `iface.contract.state-ownership`；任务：`iface.P2.model-context`, `iface.P2.tools-auth`, `iface.P4.loop`, `iface.P4.state`, `iface.P6.contracts`。
- 冻结源码：`kernel/host/src/transport/server/worker.rs:94`（`execution::input_profile::inspect`；blob `edc5d9453c1fb2f078aedb5de85bb7dd896c427a`）；`kernel/host/src/transport/model_session/execution/continuation.rs:379`（`super::input_profile::inspect(shared, commit)`；blob `622ee91951f85fb51c42877a4c95f2b56a43d1ed`）；`kernel/host/src/transport/model_session/execution/input_profile.rs:58`（`Duration::from_secs(30)`；blob `c4e20b70540246944f82ac326528f542c65b92be`）。
- 限制：30秒为检查等待上限，非新的生成deadline；检查超时不证明远端停止，合同本身禁止inspect发起I/O/推理。
- 限制：API lightweight收据实测HEAD5832 dirty WIP；仅可另作scope源码匹配，不伪造冻结0f/ae40已执行。
- 限制：生产组装与真实PilotDeck联合验收仍未完成；源码和Kernel局部PASS不关闭productionWired/jointlyVerified。

## iface.change.input-inspection-queuefull-shape

Host ScriptedSession替身原QueueFull使用Protocol/-32001且不retryable，偏离正式scripted backend错误类别。 仅把替身QueueFull对齐Resource/-32000/retryable=true，不扩展上游其它变化或新公共错误域。

- 状态归属：Kernel测试替身与正式后端错误合同。
- 合同：`iface.contract.model`, `iface.contract.product-compat`；任务：`iface.P0.upstream`, `iface.P2.model-context`, `iface.P6.contracts`。
- 冻结源码：`kernel/host/src/transport/model_session/double.rs:321`（`码/类别/retryable 同 model-endpoint-scripted`；blob `ceaf12aa697540f01eecd7fac9a07f6a8eff0441`）。
- 限制：测试double修正不是新增provider生产实现；上游范围仅此错误形状。
- 限制：API lightweight收据实测HEAD5832 dirty WIP；仅可另作scope源码匹配，不伪造冻结0f/ae40已执行。
- 限制：生产组装与真实PilotDeck联合验收仍未完成；源码和Kernel局部PASS不关闭productionWired/jointlyVerified。

## iface.change.input-inspection-lc-types-sync

lc.d.ts method union遗漏当前method表既有approval.archive与debug.dump；声明计数仍144。 LcMethod补两方法名，注释计数144→146；只登记线面类型，不新增Host方法实现或授权。

- 状态归属：Kernel L-C TypeScript类型生成/登记。
- 合同：`iface.contract.product-compat`, `iface.contract.packaging`；任务：`iface.P0.upstream`, `iface.P0.catalog`, `iface.P6.contracts`。
- 冻结源码：`kernel/iface/dist/lc.d.ts:38`（`| "approval.archive"`；blob `1e0deb51e4267f9a486a747a7fdd7d133116d9c9`）；`kernel/iface/dist/lc.d.ts:57`（`| "debug.dump"`；blob `1e0deb51e4267f9a486a747a7fdd7d133116d9c9`）。
- 限制：当前Rust检查不执行TypeScript生成/类型检查；此行仅有精确冻结source evidence，不把cargo PASS作TS消费验收。
- 限制：API lightweight收据实测HEAD5832 dirty WIP；仅可另作scope源码匹配，不伪造冻结0f/ae40已执行。
- 限制：生产组装与真实PilotDeck联合验收仍未完成；源码和Kernel局部PASS不关闭productionWired/jointlyVerified。

## iface.change.input-inspection-model-identity-registration

新增inspect_input/requirements需推进活动model身份并同步消费者/schema/API指纹；不能与crate0.1.8或旧model.6混称。 model0.1.6→0.1.7及12消费者约束/schema同步；types0.1.10/env0.1.8保持；API2119=ca0原2032逐字前缀+87候选行。

- 状态归属：Kernel活动模型接口身份与消费依赖登记。
- 合同：`iface.contract.packaging`, `iface.contract.assembly`, `iface.contract.model`；任务：`iface.P0.sources`, `iface.P0.catalog`, `iface.P2.model-context`, `iface.P6.release`。
- 冻结源码：`products/iface-identity/model/mint.toml:7`（`version = "0.1.7"`；blob `866ebe8823315a06d7c3ee01d8f87ff092319554`）；`kernel/iface/README.md:20`（`| model 身份 | 0.1.7`；blob `b90c265092acfbf3f34a7795fb543b48b5601232`）；`kernel/iface/api-baseline.txt:2064`（`ModelInputRequirements`；blob `c281c26f99f2c34180dac217ae54efe9e54e4b31`）。
- 限制：API/identity/版本/publish-plan实际运行于5832 dirty WIP，不能重标为0f或ae40冻结验证；plan不是publish。
- 限制：07 rawsourceunchanged=false由脚本期间新建_tmp fixture引起，repository输入未变；原receipt保留false。
- 限制：API lightweight收据实测HEAD5832 dirty WIP；仅可另作scope源码匹配，不伪造冻结0f/ae40已执行。
- 限制：生产组装与真实PilotDeck联合验收仍未完成；源码和Kernel局部PASS不关闭productionWired/jointlyVerified。

## iface.change.input-inspection-failed-job-pattern-fixture

0f首轮Host lib-test编译六处E0532：Failed(String)误写作unit Failed，导致新Host测试尚未运行。 ae40仅两测试文件六处Failed→Failed(_)；无生产/API变动。之后ae40双平台Host230/resources19等实际PASS，0f首编FAIL仍保留。

- 状态归属：测试状态断言与构建前提。
- 合同：`iface.contract.assembly`, `iface.contract.model`, `iface.contract.packaging`；任务：`iface.P2.model-context`, `iface.P6.contracts`。
- 冻结源码：`kernel/host/src/transport/model_session/execution/input_profile/tests.rs:220`（`ModelJobState::Failed(_)`；blob `468d7d310dcc4f9bff380152901c1ee8dfdc47e3`）；`kernel/host/src/transport/model_session/profile_tests.rs:104`（`crate::transport::ModelJobState::Failed(_)`；blob `91fa783c6820a528f90dce7243de6b8a88fa7522`）。
- 限制：0f→ae40仅test-only变动的适用性证明可继承范围，实际0f ledger10/iface218/readonly1不能称ae40重新执行。
- 限制：API lightweight收据实测HEAD5832 dirty WIP；仅可另作scope源码匹配，不伪造冻结0f/ae40已执行。
- 限制：生产组装与真实PilotDeck联合验收仍未完成；源码和Kernel局部PASS不关闭productionWired/jointlyVerified。

## 实际验证及不可互换的身份

- Linux0f：Host lib-test首编六处E0532失败，Host目标未运行；ledger10与ifACE218分别PASS；独立readonly custom1完整PASS，独立缩窄clippy PASS。
- Linuxae40：Host230（含input_profile9）与resources19实际PASS，合249标准测试；五crate fmt/clippy PASS。本候选没有重新运行readonly，0f旧结果只按test-only适用性继承。
- Mac0f：原首编E0532 FAIL保留。Macae40：Host230+resources19+ledger10实际PASS，合259标准测试；fmt和clippy仅按各argv的包集合PASS。
- API轻量13步：真实HEAD583285a6 dirty WIP，保存UTC/argv/前后源SHA与日志。默认Unsupported独立rustc负对照PASS不代表Host生产链；07 raw source_unchanged=false是运行期间生成_tmp fixture，repository_inputs_unchanged=true，原false保留。
- WIP独立snapshot的115个iface Rust源文件与0f冻结对象逐项一致，限source-match适用性，不把WIP run retag成冻结0f/ae40已测。
- API指纹2119=原ca0 2032逐字前缀+87候选行；model身份0.1.7，types/env未变。publish-plan仅plan，不声称发布。

| 实际选择 | 结果 | 受测身份 |
| --- | --- | --- | --- |
| `linux-0f081de1-first-host-model-session` | BUILD_FAILED / build | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `linux-0f081de1-first-model-session-ledger-all` | PASS / executed | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `linux-0f081de1-first-iface-all` | PASS / executed | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `linux-0f081de1-first-host-resources` | NOT_RUN_SHARED_COMPILE_FAILURE / not_run | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `linux-0f081de1-first-host-readonly-tools` | NOT_RUN_SHARED_COMPILE_FAILURE / not_run | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `linux-0f081de1-first-fmt` | PASS / build | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `linux-0f081de1-first-clippy` | NOT_RUN_SHARED_COMPILE_FAILURE / not_run | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `linux-0f081de1-independent-host-readonly-tools` | PASS / executed | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `linux-0f081de1-independent-clippy` | PASS / build | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `linux-ae40c323-first-host-model-session` | PASS / executed | `ae40c3234e16ca3d0f0e9a0bcc99f3092ddf2817` |
| `linux-ae40c323-first-host-resources` | PASS / executed | `ae40c3234e16ca3d0f0e9a0bcc99f3092ddf2817` |
| `linux-ae40c323-first-fmt` | PASS / build | `ae40c3234e16ca3d0f0e9a0bcc99f3092ddf2817` |
| `linux-ae40c323-first-clippy` | PASS / build | `ae40c3234e16ca3d0f0e9a0bcc99f3092ddf2817` |
| `mac-0f081de1-host-ledger-build-v1` | FAIL / build | `0f081de151c9b3f4c7c99b21740af4651364af28` |
| `mac-ae40c323-host-ledger-build-v1` | PASS / build | `ae40c3234e16ca3d0f0e9a0bcc99f3092ddf2817` |
| `mac-ae40c323-model-v1` | PASS / executed | `ae40c3234e16ca3d0f0e9a0bcc99f3092ddf2817` |
| `mac-ae40c323-resources-v1` | PASS / executed | `ae40c3234e16ca3d0f0e9a0bcc99f3092ddf2817` |
| `mac-ae40c323-ledger-v1` | PASS / executed | `ae40c3234e16ca3d0f0e9a0bcc99f3092ddf2817` |
| `mac-ae40c323-host-ledger-fmt-v1` | PASS / build | `ae40c3234e16ca3d0f0e9a0bcc99f3092ddf2817` |
| `mac-ae40c323-host-ledger-clippy-v1` | PASS / build | `ae40c3234e16ca3d0f0e9a0bcc99f3092ddf2817` |
| `api-01-identity-refresh` | PASS / management | `583285a6da82e33e2c8aca6638e8fae2f2433118` dirty WIP |
| `api-02-extract-candidate-api` | PASS / build | `583285a6da82e33e2c8aca6638e8fae2f2433118` dirty WIP |
| `api-03-api-strict` | PASS / build | `583285a6da82e33e2c8aca6638e8fae2f2433118` dirty WIP |
| `api-04-api-self-test` | PASS / build | `583285a6da82e33e2c8aca6638e8fae2f2433118` dirty WIP |
| `api-05-identity-check` | PASS / build | `583285a6da82e33e2c8aca6638e8fae2f2433118` dirty WIP |
| `api-06-versions-check` | PASS / build | `583285a6da82e33e2c8aca6638e8fae2f2433118` dirty WIP |
| `api-07-versions-self-test` | PASS / build | `583285a6da82e33e2c8aca6638e8fae2f2433118` dirty WIP |
| `api-08-publish-plan` | PASS / build | `583285a6da82e33e2c8aca6638e8fae2f2433118` dirty WIP |
| `api-09-diff-check` | PASS / build | `583285a6da82e33e2c8aca6638e8fae2f2433118` dirty WIP |
| `api-10-rustc-version` | PASS / build | `583285a6da82e33e2c8aca6638e8fae2f2433118` dirty WIP |
| `api-11-independent-iface-rlib` | PASS / build | `583285a6da82e33e2c8aca6638e8fae2f2433118` dirty WIP |
| `api-12-default-unsupported-compile` | PASS / build | `583285a6da82e33e2c8aca6638e8fae2f2433118` dirty WIP |
| `api-13-default-unsupported-run` | PASS / executed | `583285a6da82e33e2c8aca6638e8fae2f2433118` dirty WIP |

所有receipt、原日志、SHA、完整命令、起止时间、源与二进制身份位于JSON；proposedRunId仅供主ledger唯一维护者关联，本任务未写主ledger。五层建议中productionWired和jointlyVerified统一UNKNOWN。
