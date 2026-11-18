# scoped HTTP后续修复：276 → aba → 52

本文件为独立后续源，不修改8a2或更早已冻结来源。产品source evidence只读三个冻结Git对象；公开准备token API尚WIP，不纳已实现。未运行Cargo，未写主ledger/远端。

| 冻结提交 | 实际状态 |
| --- | --- |
| `8a2e8b57a47cbf591b59524c7aff42fe22b0f520` | 原生产首编E0599 FAIL；原文件身份不改 |
| `276d0a315ac396f3017aa0598f22601fbaaefdef` | no-default生产PASS、external6实际PASS，lib-test五路径错误FAIL；default延后 |
| `aba64691bbba5515e5dc4de136adf08c50942721` | cleanup路径修复已实际编译；trusted6PASS；model260=234PASS26FAIL，http_scope22子集PASS；Clippy2FAIL |
| `52a728253137297fac8d686066380ba83530d0b1` | graceful排空修复刚冻结，NOT_RUN |

| 稳定ID | 改动 | 首次修复 | 类别 |
| --- | --- | --- | --- |
| `iface.change.managed-catalog-startup-error-message` | 启动目录错误使用正式message字段解除首编阻塞 | `276d0a315ac396f3017aa0598f22601fbaaefdef` | Kernel生产构建修复 |
| `iface.change.http-cleanup-test-module-paths` | cleanup测试五处module路径修正 | `aba64691bbba5515e5dc4de136adf08c50942721` | 测试前提修正（非生产fix） |
| `iface.change.graceful-close-drains-admitted-queue` | 优雅关闭排空原准入queued generation后再close | `52a728253137297fac8d686066380ba83530d0b1` | Kernel生产生命周期修复 |
| `iface.change.graceful-close-tracks-duplex-boundaries` | duplex已接受boundary记账并纳入graceful排空 | `52a728253137297fac8d686066380ba83530d0b1` | Kernel生产生命周期修复 |

## iface.change.managed-catalog-startup-error-message

8a2生产首编E0599：iface Error不实现Display/ToString，managed.install错误转换调用error.to_string无法编译。 仅改为std::io::Error::other(error.message)，保留正式错误消息并使生产组装能编译；不改catalog准入与原子发布逻辑。

- 状态归属：Kernel managed模型启动错误传播。
- 合同：`iface.contract.assembly`, `iface.contract.atomic-install`, `iface.contract.model`, `iface.contract.packaging`；任务：`iface.P2.embedding`, `iface.P2.model-context`, `iface.P2.atomic-install`, `iface.P6.contracts`。
- 最新52冻结源码：`kernel/host/src/transport/server/assemble.rs:224`（`std::io::Error::other(error.message)`；blob `c543e638dd4fbb7e6bec7624ebe36bb2f9e1657e`）。
- 首次修复源码的独立commit/行/blob/SHA在JSON firstFixSourceEvidence，不用最新源码冒充首修。
- 限制：276 production(features[])和external6PASS证明该候选局部恢复；276 lib-test随后暴露独立测试路径错误，不称整宿主全量通过。
- 限制：公开token API登记仍属其他任务WIP，不在这三个冻结对象已实现项中。
- 限制：生产组装/全产品联合验收未完成；productionWired/jointlyVerified不自动提升。

## iface.change.http-cleanup-test-module-paths

276 lib-test五处E0425/E0433：嵌套cleanup测试使用错误super层级引用claim_commit/release_claim/commit_guard fixtures，导致Host lib测试不能执行。 改用crate::transport::model_session::execution全路径，精准修四个claim/release调用及一个fixtures::job引用；只改测试文件，不改变HTTP授权/清理生产机制。

- 状态归属：Kernel scoped HTTP测试命名空间。
- 合同：`iface.contract.model`, `iface.contract.assembly`, `iface.contract.packaging`；任务：`iface.P2.model-context`, `iface.P6.contracts`。
- 最新52冻结源码：`kernel/host/src/transport/model_session/execution/http_scope/tests/cleanup.rs:38`（`let claim = crate::transport::model_session::execution::claim_commit(`；blob `5618e14bad6b1fd20914d521f9810554d378bb28`）；`kernel/host/src/transport/model_session/execution/http_scope/tests/cleanup.rs:237`（`crate::transport::model_session::execution::commit_guard::fixtures::job(`；blob `5618e14bad6b1fd20914d521f9810554d378bb28`）。
- 首次修复源码的独立commit/行/blob/SHA在JSON firstFixSourceEvidence，不用最新源码冒充首修。
- 限制：aba仅test path修正，不能计为修复生产权限或资源释放行为；Linuxaba实际结果已独立追加本文件，仍保留失败。
- 限制：公开token API登记仍属其他任务WIP，不在这三个冻结对象已实现项中。
- 限制：生产组装/全产品联合验收未完成；productionWired/jointlyVerified不自动提升。

## iface.change.graceful-close-drains-admitted-queue

8a2/276仅允许已claim的HTTP在途drain；尚未claim的已准入generation被close_requested拒绝，且close只等commit fence，可能先关backend。 新增submit_reserved关闭后拒新准入；authorize_commit允许Closing+close_requested下原permit队列继续派发；has_drainable_work汇合Queued/Running generation及fence，async close有界锁外等待后才backend.close，同步close返回pending。Unknown不阻断cleanup尝试，仍不伪造释放。

- 状态归属：Kernel已接受generation、关闭请求与后端close时序。
- 合同：`iface.contract.model`, `iface.contract.lifecycle`, `iface.contract.state-ownership`, `iface.contract.auth`；任务：`iface.P2.model-context`, `iface.P2.history-lifecycle`, `iface.P4.loop`, `iface.P4.state`, `iface.P6.contracts`。
- 最新52冻结源码：`kernel/host/src/transport/model_session/execution.rs:218`（`ModelNotReady: model session is closing`；blob `69bd0a10849f8757638b15c00262ab5a22ba09b9`）；`kernel/host/src/transport/model_session/execution/commit_guard.rs:111`（`Closing forbids new admission`；blob `2e513887356d83024edffad67d0f71d0810973f3`）；`kernel/host/src/transport/model_session/registry.rs:165`（`pub(crate) fn has_drainable_work`；blob `fb0e5a3830719f771c84f20e0eeaf416a3a29ffe`）；`kernel/host/src/transport/model_session/async_ops/finish.rs:228`（`let pending = {`；blob `736054eff11256e5d0bca1947c21f381a23d098a`）；`kernel/host/src/transport/model_session/handlers_session/finish.rs:84`（`if commit_fence.is_some() || (!abort && admitted_generation)`；blob `404caa480f982bed78312f09f7fa76484f36d48a`）。
- 首次修复源码的独立commit/行/blob/SHA在JSON firstFixSourceEvidence，不用最新源码冒充首修。
- 限制：这是52才新增的排队generation行为；8a2文档的“只证已claim”事实保持原样，不回标旧候选。
- 限制：仍复核原owner/policy/lease/取消/期限/父执行；Closing例外不授予新准入或永久执行权。
- 限制：BACKEND_WAIT有界等待超时返回close_pending而不是宣布Closed/Released；Unknown仍保留资源占用，cleanup调用结果需真实事实。
- 限制：公开token API登记仍属其他任务WIP，不在这三个冻结对象已实现项中。
- 限制：生产组装/全产品联合验收未完成；productionWired/jointlyVerified不自动提升。

## iface.change.graceful-close-tracks-duplex-boundaries

duplex boundary不占新generation槽，单看generation/fence可能漏掉已入队但尚未settle的boundary；close不能提前关闭其backend或让已接受边界因Closing被拒。 pending_boundaries以(job,principal,input)登记入队，send失败撤销，成功/派发前拒绝/明确错误均settle；has_drainable_work汇合pending；continuation允许Closing下旧boundary在同scope原准入约束内完成，新boundary提交仍拒绝。

- 状态归属：Kernel已接受duplex边界与generation共同所有权。
- 合同：`iface.contract.model`, `iface.contract.lifecycle`, `iface.contract.state-ownership`, `iface.contract.context`；任务：`iface.P2.model-context`, `iface.P2.history-lifecycle`, `iface.P4.loop`, `iface.P4.state`, `iface.P6.contracts`。
- 最新52冻结源码：`kernel/host/src/transport/model_session/registry.rs:140`（`pub(crate) pending_boundaries:`；blob `fb0e5a3830719f771c84f20e0eeaf416a3a29ffe`）；`kernel/host/src/transport/model_session/execution/continuation.rs:139`（`let boundary_key =`；blob `83980e754bc771aae65d3f6af4829ed5a12c2cb6`）；`kernel/host/src/transport/model_session/execution/continuation.rs:276`（`fn settle_boundary(`；blob `83980e754bc771aae65d3f6af4829ed5a12c2cb6`）；`kernel/host/src/transport/model_session/execution/continuation.rs:192`（`previously accepted boundary`；blob `83980e754bc771aae65d3f6af4829ed5a12c2cb6`）。
- 首次修复源码的独立commit/行/blob/SHA在JSON firstFixSourceEvidence，不用最新源码冒充首修。
- 限制：不新建generation、不重置scope期限；队列前后授权、原job仍Running等围栏继续存在。
- 限制：52相关回归源码存在但未执行，不从测试断言推导PASS。
- 限制：公开token API登记仍属其他任务WIP，不在这三个冻结对象已实现项中。
- 限制：生产组装/全产品联合验收未完成；productionWired/jointlyVerified不自动提升。

## 276实际收据及继承边界

276生产、external6的实际Cargo features为`[]`。external6包括managed3，但不能据6项PASS把Host lib的trusted_egress/http_scope等未执行组当作通过。lib-test首败有五处E0425/E0433，aba只修测试路径；该候选实际trusted6PASS、model260中234PASS26FAIL、http22子集PASS且Clippy2FAIL；52才另外修改生产graceful行为，两者不可混算。

runtime async_dial8、iface same_source1及runtime+iface Clippy的原实测commit仍为8a2。source tree相同的适用性证明可引用，不能重新标276/aba/52执行通过。最新52的四项candidateVerification均NOT_RUN。

| 原实际选择 | 结果/类别 | 实测commit |
| --- | --- | --- |
| `host-production-native-only` | FAIL / build | `8a2e8b57a47cbf591b59524c7aff42fe22b0f520` |
| `276-host-production-native-only` | PASS / build | `276d0a315ac396f3017aa0598f22601fbaaefdef` |
| `276-native-only-external-backends` | PASS / executed | `276d0a315ac396f3017aa0598f22601fbaaefdef` |
| `276-native-only-trusted-egress` | FAIL / build | `276d0a315ac396f3017aa0598f22601fbaaefdef` |
| `276-host-model-session` | NOT_RUN / not_run | `276d0a315ac396f3017aa0598f22601fbaaefdef` |
| `276-fmt-host` | PASS / build | `276d0a315ac396f3017aa0598f22601fbaaefdef` |
| `276-clippy-host-native-only` | NOT_RUN / not_run | `276d0a315ac396f3017aa0598f22601fbaaefdef` |

所有UTC、argv、receipt/log SHA、产物features与前后SHA在JSON；已有8a2首败只引用不重复执行计数。待aba完成可仅追加本新源而不修改旧冻结文件。生产正式组装和联合验收继续UNKNOWN。

## aba实际封存结果（与52静态修复分开）

aba源码/产物前后SHA保持原身份：266次标准执行=trusted_egress6+model_session260，240PASS、26FAIL、0ignore。http_scope22全部PASS是260内子集，不能另加分母。26失败的共同原因是`external model sessions require explicit model and endpoint_ref`：async_integration_tests.rs:67有22项，tool_results/tests/async_path.rs:310有4项，均在open被拒而未进入原本目标场景。

Host fmt PASS；no-default all-targets Clippy FAIL两项：registry.rs:714的admit_generation参数8/7和http_scope/tests.rs:330 unused_mut。作者后续fixture/Clippy修复另行冻结/测试，不回标aba，更不与52 graceful静态代码混称通过。default矩阵按root要求延后。

| aba原选择 | 结果 | 精确范围 |
| --- | --- | --- |
| `aba-native-only-trusted-egress` | PASS / executed | aba no-default trusted_egress6实际PASS，二进制实际features[]，五路径编译阻塞解除。 |
| `aba-host-model-session` | FAIL / executed | aba model_session260实际执行234PASS/26FAIL/0ignore；http_scope22是260内子集全PASS不重复加数。26失败在session_open即被显式binding合同拒绝，未到达原目标场景。 |
| `aba-fmt-host` | PASS / build | aba精确原命令/选择范围，不回标52最新候选 |
| `aba-clippy-host-native-only` | FAIL / build | aba no-default Host all-targets Clippy FAIL两项：registry admit_generation args8/7，http_scope/tests.rs unused_mut；不称生产运行失败。 |

上述结果只回补这个新source；旧8a2/276引用及实测身份未改。52四项currentCandidateVerification依旧NOT_RUN，生产联合验收UNKNOWN。
