# 8a2e scoped pull HTTP与受信模型组装独立追踪

冻结起点`0291dbe02651e51e40d8f1689fe6f2e0c2930693`，候选`8a2e8b57a47cbf591b59524c7aff42fe22b0f520`（88路径）。本文件按13个逻辑改动归并，不按文件计修复。只读git show冻结对象，未读准备token后续WIP作为已实现；旧来源保持SHA，未运行Cargo、未写主ledger/remote。

交付前收到真实首批收据并回补：8a2e native-only Host生产首编E0599 FAIL；独立runtime async_dial8及iface same_source1 PASS，三包fmt与runtime/iface Clippy PASS；Host相关选择未运行。测试源码22+14+managed3与静态Astra审查不是PASS。API15步实际0291 dirty WIP，严格保留旧身份。

| 稳定ID | 逻辑改动 | 类别 | 当前验证 |
| --- | --- | --- | --- |
| `iface.change.public-scoped-http-pull` | 公开惰性HTTP pull及attempt结果合同 | 公开ifACE API变更 | PUBLIC_IFACE_COMPILE_CHECK_PASS_HOST_BLOCKED |
| `iface.change.public-http-close-scope-token-identity` | 公开cleanup-only close_scope及同源令牌比较 | 公开ifACE API变更 | SCOPED_TOKEN_TEST_PASS_HOST_BLOCKED |
| `iface.change.http-current-permit-poll-fence` | 每次HTTP派发及poll复核真实原permit和token | Kernel生产路径与内部修复 | HOST_BUILD_FAILED_TESTS_NOT_RUN |
| `iface.change.http-graceful-claimed-generation-drain` | 优雅关闭保留已claim在途generation的HTTP排空 | Kernel生命周期行为 | HOST_BUILD_FAILED_TESTS_NOT_RUN |
| `iface.change.http-scope-linearized-cleanup` | scope关闭线性化并汇合真实attempt资源释放 | Kernel内部生命周期实现 | HOST_BUILD_FAILED_TESTS_NOT_RUN |
| `iface.change.trusted-async-egress-pool` | 受信异步拨号器与按真实scope隔离的池 | Kernel可信组装资源面 | HOST_BUILD_FAILED_TESTS_NOT_RUN |
| `iface.change.async-egress-dns-peer-accounting` | 异步DNS/固定peer复核与真实helper记账 | Kernel内部出站实现 | RUNTIME_SCOPED_8_PASS_HOST_BLOCKED |
| `iface.change.typed-egress-error-release-facts` | typed网络错误保留真实source并区分释放事实 | Kernel内部错误及资源合同 | RUNTIME_ERROR_SOURCE_TESTS_PASS_HOST_BLOCKED |
| `iface.change.managed-model-startup-catalog-atomic` | 公开受信模型builder与目录工厂原子发布 | Kernel公开Host组装及生产接线 | HOST_BUILD_FAILED_TESTS_NOT_RUN |
| `iface.change.managed-effective-binding-admission` | 批准binding解析及真实model身份准入 | Kernel内部准入修复 | HOST_BUILD_FAILED_TESTS_NOT_RUN |
| `iface.change.model-update-out-of-order-binding-fence` | 乱序update回复不能覆盖新有效配置 | Kernel内部并发修复 | HOST_BUILD_FAILED_TESTS_NOT_RUN |
| `iface.change.managed-cross-provider-model-name-gap` | 同模型名跨provider明确拒绝，保留P5缺口 | 产品兼容缺口与拒绝行为 | HOST_BUILD_FAILED_TESTS_NOT_RUN |
| `iface.change.http-pull-identity-registration` | pull合同登记env0.1.9/types0.1.11及API2198 | 兼容版本登记 | COMPILE_CHECK_PASS_REGISTRATION_WIP_ONLY |

## iface.change.public-scoped-http-pull

push sink不能让产品沿用原异步head/wire/body idle仲裁、重试/redirect及可观察真实释放；字符串错误也不能决定是否派发。 新增HttpPullRequest/Context/Head/Body/Exchange、Data/Progress、HttpDispatch/End/Resources/Receipt及分phase/kind失败；start惰性、poll才可I/O，非2xx仍HTTP响应，Err须配真实control receipt。

- 状态归属：ifACE端口合同；产品connector持有实际HTTP attempt。
- 合同：`iface.contract.model`, `iface.contract.auth`, `iface.contract.lifecycle`, `iface.contract.state-ownership`；任务：`iface.P2.model-context`, `iface.P3.model`, `iface.P4.composition`, `iface.P6.contracts`。
- 精确冻结源码：`kernel/iface/src/env/http_pull.rs:168`（`pub trait HttpPullTransport`；blob `a813bf3f4e9230cb87cb8ec8fab5eff9faa76eed`）；`kernel/iface/src/env/http_pull.rs:55`（`pub trait HttpBody`；blob `a813bf3f4e9230cb87cb8ec8fab5eff9faa76eed`）；`kernel/iface/src/env/http_pull.rs:90`（`pub struct HttpExchangeReceipt`；blob `a813bf3f4e9230cb87cb8ec8fab5eff9faa76eed`）；`kernel/iface/src/env/http_pull.rs:45`（`pub enum HttpFrame`；blob `a813bf3f4e9230cb87cb8ec8fab5eff9faa76eed`）。
- 限制：端口依赖types而不依赖model_session/Tokio/Hyper；合同不实现proxy/CA/encoding或供应商协议。
- 限制：Data必须非空；Progress须真实新wire字节，不能用缓冲尾部/空转延长idle。正常EOF不等于资源Released。
- 限制：首编E0599持续保留；本项精确验证范围见末节，Host未执行测试不标通过。
- 限制：0291 dirty WIP的15步API收据不是8a2e冻结Host/真实I/O运行；静态审查和源码测试数量也不构成执行通过。

## iface.change.public-http-close-scope-token-identity

提前终止后若等待模型终态才能收池会循环等待；公开context字符串或新造token不能清理/重新派发别的作业。 HttpPullTransport.close_scope默认Unsupported，返回可多等待者cleanup handle；OperationCancel.same_source仅比Arc身份，不自行赋权；原scope撤权/终态后仍允许可信清理。

- 状态归属：ifACE清理合同与共享取消身份。
- 合同：`iface.contract.model`, `iface.contract.lifecycle`, `iface.contract.state-ownership`, `iface.contract.auth`；任务：`iface.P2.model-context`, `iface.P2.history-lifecycle`, `iface.P4.state`, `iface.P6.contracts`。
- 精确冻结源码：`kernel/iface/src/env/http_pull.rs:177`（`fn close_scope(`；blob `a813bf3f4e9230cb87cb8ec8fab5eff9faa76eed`）；`kernel/iface/src/env/http_pull.rs:109`（`pub trait HttpScopeCleanup`；blob `a813bf3f4e9230cb87cb8ec8fab5eff9faa76eed`）；`kernel/iface/src/types/operation_cancel.rs:31`（`pub fn same_source`；blob `216f2a7ac5f5684a4923eabfccccfd4bda9c08ba`）。
- 限制：cleanup不能取消job token、派发新I/O或影响其他job；Unsupported/Unknown不冒充Released。
- 限制：API extractor不采集same_source inherent方法；WIP独立consumer只证明公开方法可用，不证明Host授权。
- 限制：首编E0599持续保留；本项精确验证范围见末节，Host未执行测试不标通过。
- 限制：0291 dirty WIP的15步API收据不是8a2e冻结Host/真实I/O运行；静态审查和源码测试数量也不构成执行通过。

## iface.change.http-current-permit-poll-fence

仅凭scope的同名字符串/新造token或初次准入不能允许撤权/epoch变化后继续HTTP；长期池复用需逐次检查。 claim时保存原CommitPermit及backend Weak；scope逐项核同源token、job/session/model-session/principal/deadline、原backend、input/epoch/代次、Running、取消/父执行/委派和当前owner/policy；head/body每poll复核，底层poll/stop/waker在HostState锁外。

- 状态归属：Kernel原准入凭据、generation与当前授权。
- 合同：`iface.contract.auth`, `iface.contract.model`, `iface.contract.lifecycle`, `iface.contract.state-ownership`；任务：`iface.P2.tools-auth`, `iface.P2.model-context`, `iface.P4.loop`, `iface.P4.state`, `iface.P6.contracts`。
- 精确冻结源码：`kernel/host/src/transport/model_session/execution/http_scope.rs:22`（`pub(crate) struct HttpAuthority`；blob `f036ea611033c40d33b6ddb97f3ec3d8d2fa5184`）；`kernel/host/src/transport/model_session/execution/http_scope.rs:90`（`if !original.cancel().same_source`；blob `f036ea611033c40d33b6ddb97f3ec3d8d2fa5184`）；`kernel/host/src/transport/model_session/execution/commit_guard.rs:145`（`pub(super) fn authorize_current_permit`；blob `9e9c5006af0a4a6ba7e5c2e1dfe85ac7c99d7a2c`）；`kernel/host/src/transport/model_session/execution/http_scope.rs:327`（`struct GuardedHead`；blob `f036ea611033c40d33b6ddb97f3ec3d8d2fa5184`）；`kernel/host/src/transport/model_session/execution/http_scope.rs:381`（`struct GuardedBody`；blob `f036ea611033c40d33b6ddb97f3ec3d8d2fa5184`）。
- 限制：8a2e没有后续WIP准备token方案；只跟踪此处claim后HttpAuthority路径。
- 限制：重试/redirect/pool复用须调用受信pool.authorize，不能靠已验证句柄绕过最新权限。
- 限制：首编E0599持续保留；本项精确验证范围见末节，Host未执行测试不标通过。
- 限制：0291 dirty WIP的15步API收据不是8a2e冻结Host/真实I/O运行；静态审查和源码测试数量也不构成执行通过。

## iface.change.http-graceful-claimed-generation-drain

D96 session.close停止新输入但已接受在途generation仍可能需要首次HTTP、重试/fallback；不得把close当作远端取消事实。 HTTP scope不因D96 close_requested单独拒绝已claim且Running的原generation；仍严格受cancel/deadline/撤权/父结束/Unknown/宿主shutdown和显式scope关闭约束。

- 状态归属：Kernel D96关闭与已claim生成生命周期。
- 合同：`iface.contract.model`, `iface.contract.lifecycle`, `iface.contract.state-ownership`；任务：`iface.P2.history-lifecycle`, `iface.P2.model-context`, `iface.P4.loop`, `iface.P4.state`, `iface.P6.contracts`。
- 精确冻结源码：`kernel/host/src/transport/model_session/execution/http_scope.rs:86`（`generation has not been dispatched`；blob `f036ea611033c40d33b6ddb97f3ec3d8d2fa5184`）；`kernel/host/src/transport/model_session/execution/http_scope.rs:140`（`!matches!(job.state, ModelJobState::Running)`；blob `f036ea611033c40d33b6ddb97f3ec3d8d2fa5184`）；`kernel/host/src/transport/model_session/execution/commit_guard.rs:111`（`if !record.is_ready() || record.close_requested`；blob `9e9c5006af0a4a6ba7e5c2e1dfe85ac7c99d7a2c`）；`kernel/host/src/transport/model_session/execution/http_scope/tests.rs:243`（`fn graceful_close_drains_accepted_generation_including_initial_dispatch_and_retry`；blob `27279104366fff97bf16ccd05f64e3335df80f2f`）。
- 限制：准确边界是已claim的在途generation；commit_guard仍拒绝close_requested后尚未claim的排队generation，不能泛称所有已commit排队任务都可首次派发。
- 限制：对应测试源码存在但本候选未执行，不计通过。
- 限制：首编E0599持续保留；本项精确验证范围见末节，Host未执行测试不标通过。
- 限制：0291 dirty WIP的15步API收据不是8a2e冻结Host/真实I/O运行；静态审查和源码测试数量也不构成执行通过。

## iface.change.http-scope-linearized-cleanup

start在锁外调用raw.start时可与close竞态；丢弃观察句柄/关闭标志不能证明body/socket/helper真实退出，旧scope迟到清理不能关新generation。 Scope.start/close共享closing锁和starts计数；迟到control仍登记并stop，待raw.start归还后才raw.close_scope；generation保存http_scope_closed阻止重开；cleanup汇合raw scope及全部attempt receipts，零成功start按惰性合同为空，Unsupported保持closed+Unknown。

- 状态归属：Kernel scope registry与connector真实资源回执。
- 合同：`iface.contract.lifecycle`, `iface.contract.state-ownership`, `iface.contract.auth`, `iface.contract.model`；任务：`iface.P2.history-lifecycle`, `iface.P4.state`, `iface.P4.loop`, `iface.P6.contracts`。
- 精确冻结源码：`kernel/host/src/transport/model_session/execution/http_scope/cleanup.rs:104`（`pub(super) fn begin(`；blob `29c75ee0693c4a21231f2c0a68775b9647c2edb9`）；`kernel/host/src/transport/model_session/execution/http_scope/cleanup.rs:153`（`impl Starting`；blob `29c75ee0693c4a21231f2c0a68775b9647c2edb9`）；`kernel/host/src/transport/model_session/execution/http_scope/cleanup.rs:232`（`impl HttpScopeCleanup for Cleanup`；blob `29c75ee0693c4a21231f2c0a68775b9647c2edb9`）；`kernel/host/src/transport/model_session/execution/http_scope.rs:307`（`generation.http_scope_closed = true;`；blob `f036ea611033c40d33b6ddb97f3ec3d8d2fa5184`）。
- 限制：attempt Released仅本attempt资源或安全归池；池/reactor/helper收敛另由Host生命周期确认，不等于远端模型停止。
- 限制：cleanup查询、停止和等待不受业务撤权门阻断，但仍必须原token身份。
- 限制：首编E0599持续保留；本项精确验证范围见末节，Host未执行测试不标通过。
- 限制：0291 dirty WIP的15步API收据不是8a2e冻结Host/真实I/O运行；静态审查和源码测试数量也不构成执行通过。

## iface.change.trusted-async-egress-pool

普通Model包不应持裸网络能力；HTTP client池复用只携URI，不能猜某次attempt身份或跨job借权限。 TrustedEgressDialer.pool经真实scope校验签发池；same_scope比宿主句柄；authorize同时检查logical/physical完整scheme端点；connect返回不可提裸socket的TrustedTcpStream，异步读写继续核权；单attempt stop不得停整池。

- 状态归属：受信Host拨号器、scope池与socket所有权。
- 合同：`iface.contract.auth`, `iface.contract.model`, `iface.contract.lifecycle`, `iface.contract.assembly`；任务：`iface.P2.tools-auth`, `iface.P3.model`, `iface.P4.composition`, `iface.P6.contracts`。
- 精确冻结源码：`kernel/host/src/startup/trusted_egress.rs:34`（`pub struct TrustedEgressDialer`；blob `269f73d6963659b905753c827bcd3c7043e38a3f`）；`kernel/host/src/startup/trusted_egress.rs:122`（`pub fn same_scope`；blob `269f73d6963659b905753c827bcd3c7043e38a3f`）；`kernel/host/src/startup/trusted_egress.rs:133`（`pub fn authorize(`；blob `269f73d6963659b905753c827bcd3c7043e38a3f`）；`kernel/host/src/startup/trusted_egress.rs:382`（`pub struct TrustedTcpStream`；blob `269f73d6963659b905753c827bcd3c7043e38a3f`）。
- 限制：产品connector只在可信组装层获得拨号器；普通factory只持guarded ifACE pull port。
- 限制：跨job连接共享不在本实现内，连接池行为/性能/产品兼容需另测。
- 限制：首编E0599持续保留；本项精确验证范围见末节，Host未执行测试不标通过。
- 限制：0291 dirty WIP的15步API收据不是8a2e冻结Host/真实I/O运行；静态审查和源码测试数量也不构成执行通过。

## iface.change.async-egress-dns-peer-accounting

取消等待future不能终止系统resolver；逐poll起DNS会无界扩任务；代理remote DNS不应本地解析logical域名，连接不能再次DNS脱离审计peer。 沿唯一EgressGate在DNS前和每实际connect前复核logical+physical；只解析physical，审核SocketAddr后固定该peer；loopback外逸拒绝。DNS每次一个spawn_blocking、进程32并发worker，真实worker持lease直到退出；TCP timeout从DNS返回后计算。

- 状态归属：Kernel唯一出站判决与DNS/拨号helper事实。
- 合同：`iface.contract.auth`, `iface.contract.lifecycle`, `iface.contract.state-ownership`, `iface.contract.model`；任务：`iface.P2.tools-auth`, `iface.P3.model`, `iface.P4.state`, `iface.P6.contracts`。
- 精确冻结源码：`kernel/runtime/src/egress/async_dial.rs:152`（`pub async fn connect_tcp_async`；blob `833ba7b292c393100a6a39273a7bca9c385e3e58`）；`kernel/runtime/src/egress/async_dial.rs:115`（`async fn resolve_with<F>`；blob `833ba7b292c393100a6a39273a7bca9c385e3e58`）；`kernel/runtime/src/egress/async_dial.rs:77`（`pub struct AsyncEgressWork`；blob `833ba7b292c393100a6a39273a7bca9c385e3e58`）；`kernel/runtime/src/egress/async_dial.rs:214`（`TcpStream::connect(address)`；blob `833ba7b292c393100a6a39273a7bca9c385e3e58`）。
- 限制：无额外DNS/TLS产品超时；不改变域名白名单为域名+IP双ACL，当前无独立IP/CIDR ACL，不能称完整DNS rebinding防护。
- 限制：work.is_idle仅helper/排队拨号清空，不独立证明HTTP attempt或整个池Released。
- 限制：首编E0599持续保留；本项精确验证范围见末节，Host未执行测试不标通过。
- 限制：0291 dirty WIP的15步API收据不是8a2e冻结Host/真实I/O运行；静态审查和源码测试数量也不构成执行通过。

## iface.change.typed-egress-error-release-facts

人类错误文本猜ENOTFOUND会错分provider/系统原因；EOF、丢句柄、取消或typed错误本身不证明实际资源都释放。 新增AsyncEgressError保存std::io::Error source/kind，Host按结构化kind映射网络码，未分类resolver code留空；pool Released必须stopped、work idle、monitor退出且所有socket关闭，resident排空计入attempt/egress/lifecycle。

- 状态归属：Kernel错误归因、scope池与resident排空。
- 合同：`iface.contract.auth`, `iface.contract.model`, `iface.contract.lifecycle`, `iface.contract.state-ownership`；任务：`iface.P2.tools-auth`, `iface.P2.history-lifecycle`, `iface.P4.state`, `iface.P6.contracts`。
- 精确冻结源码：`kernel/runtime/src/egress/async_dial.rs:22`（`pub enum AsyncEgressError`；blob `833ba7b292c393100a6a39273a7bca9c385e3e58`）；`kernel/host/src/startup/trusted_egress.rs:477`（`fn map_egress(`；blob `269f73d6963659b905753c827bcd3c7043e38a3f`）；`kernel/host/src/startup/trusted_egress.rs:304`（`fn released(&self) -> bool`；blob `269f73d6963659b905753c827bcd3c7043e38a3f`）；`kernel/host/src/transport/server/control.rs:173`（`let managed_pending = self.managed_model_pending();`；blob `50ddf59941b6b219ff3f3419b0f37f563ec12dc5`）。
- 限制：不替产品判retryable/provider业务错误；普通raw std::io cause仍须可信connector使用同一秘密表过滤。
- 限制：同步既有EgressError保持原构造/匹配，不将新typed async source冒称全部同步出口已迁移。
- 限制：首编E0599持续保留；本项精确验证范围见末节，Host未执行测试不标通过。
- 限制：0291 dirty WIP的15步API收据不是8a2e冻结Host/真实I/O运行；静态审查和源码测试数量也不构成执行通过。

## iface.change.managed-model-startup-catalog-atomic

外部HTTP/工厂需要真实Shared后才能验证scope，若worker已启动或目录写半份再失败会暴露默认/半激活后端。 新增serve_resident_with_managed_model和TrustedModelSetup.catalog/transport/factory；Shared创建后worker/accept前锁外builder，目录全量校验后同一HostState锁安装models+approved bindings+factory；配置/预建factory冲突ready前拒绝，无默认回退。

- 状态归属：Kernel启动组合根、批准目录与工厂发布。
- 合同：`iface.contract.assembly`, `iface.contract.atomic-install`, `iface.contract.model`, `iface.contract.packaging`；任务：`iface.P2.embedding`, `iface.P2.model-context`, `iface.P2.atomic-install`, `iface.P4.composition`, `iface.P6.contracts`。
- 精确冻结源码：`kernel/host/src/startup/embed.rs:258`（`pub fn serve_resident_with_managed_model`；blob `0db3672e30ff0f95e120682a8518cd4393d31770`）；`kernel/host/src/startup/managed_model.rs:70`（`pub trait TrustedModelSetup`；blob `23d59afa300c58421ea9c74d47b17d2427efacb1`）；`kernel/host/src/startup/managed_model.rs:94`（`pub(crate) fn install(`；blob `23d59afa300c58421ea9c74d47b17d2427efacb1`）；`kernel/host/src/transport/model_session/registry.rs:34`（`pub(crate) fn install_managed_catalog`；blob `2183c3e8d499c34b539bf9c6c1512de209c6ce21`）；`kernel/host/src/transport/server/assemble.rs:216`（`Shared exists but no worker/accept can dispatch yet`；blob `c5baa69d788b3b75925b8e5bc8f7d3756ebd4cbe`）。
- 限制：builder失败前未交Host资源由builder自行清理；已交transport若factory/catalog失败Host保owner并有界stop_and_wait，超时明确Unknown。
- 限制：旧serve_resident/ExternalBackends源码形状保持，但WIP静态token比较不等于真实Host编译。
- 限制：首编E0599持续保留；本项精确验证范围见末节，Host未执行测试不标通过。
- 限制：0291 dirty WIP的15步API收据不是8a2e冻结Host/真实I/O运行；静态审查和源码测试数量也不构成执行通过。

## iface.change.managed-effective-binding-admission

缺省scripted或endpoint probe推导model_id会把外部请求记到错误模型，public model/endpoint不能自报绕目录；update只能按能力支持字段更改。 builtin固定真实binding；外部必须显式model+endpoint_ref并精确匹配trusted catalog；effective_binding仅后端确认后生效，generation冻结binding/revision，admit_generation按真实binding.model入共同账本，受信resolver返回原generation事实。

- 状态归属：Kernel批准目录、有效模型配置及共同准入账本。
- 合同：`iface.contract.model`, `iface.contract.auth`, `iface.contract.state-ownership`, `iface.contract.assembly`；任务：`iface.P2.model-context`, `iface.P3.model`, `iface.P4.state`, `iface.P6.contracts`。
- 精确冻结源码：`kernel/host/src/transport/model_session/handlers_parse.rs:86`（`pub(crate) fn resolve_binding`；blob `09a1dd5a33ba866279b9dd7e08415b393f2fcad8`）；`kernel/host/src/transport/model_session/handlers_parse.rs:126`（`pub(crate) fn validate_binding_update`；blob `09a1dd5a33ba866279b9dd7e08415b393f2fcad8`）；`kernel/host/src/transport/model_session/execution.rs:261`（`&binding.model)?;`；blob `1cdc580f803ae92fe9f123a1b504b8623cd4b42c`）；`kernel/host/src/startup/managed_model.rs:28`（`pub struct TrustedModelBindingResolver`；blob `23d59afa300c58421ea9c74d47b17d2427efacb1`）。
- 限制：model request JSON中的provider不授权新端点；credentials仍由批准origin的可信connector绑定。
- 限制：实际产品多provider逻辑名兼容缺口另列，不能据真实binding准入声称全产品兼容。
- 限制：首编E0599持续保留；本项精确验证范围见末节，Host未执行测试不标通过。
- 限制：0291 dirty WIP的15步API收据不是8a2e冻结Host/真实I/O运行；静态审查和源码测试数量也不构成执行通过。

## iface.change.model-update-out-of-order-binding-fence

ConfigApplied先到、旧update future后返回时，旧成功/错误可能确认后续pending或回滚较新binding；backend替换/终态也不能被旧回复推进。 保留pending_binding(revision,binding)，拒第二个pending update；finish_update核backend Arc/id、终态和本plan owns_pending/already_confirmed；只确认精确revision；迟到成功/失败拒或不改新owner，ConfigApplied同样要求当前pending匹配。

- 状态归属：Kernel配置更新所有权与有效revision/binding。
- 合同：`iface.contract.model`, `iface.contract.context`, `iface.contract.state-ownership`, `iface.contract.lifecycle`；任务：`iface.P2.model-context`, `iface.P4.state`, `iface.P6.contracts`。
- 精确冻结源码：`kernel/host/src/transport/model_session/async_ops/control.rs:35`（`ModelSessionUpdatePending`；blob `b8377481031de8e9980eb70dc5e6eb659f738517`）；`kernel/host/src/transport/model_session/async_ops/control.rs:150`（`fn finish_update(`；blob `b8377481031de8e9980eb70dc5e6eb659f738517`）；`kernel/host/src/transport/model_session/async_ops/control.rs:181`（`let owns_pending`；blob `b8377481031de8e9980eb70dc5e6eb659f738517`）；`kernel/host/src/transport/model_session/registry.rs:154`（`pub(crate) fn confirm_binding`；blob `2183c3e8d499c34b539bf9c6c1512de209c6ce21`）。
- 限制：四个update_race测试为源码存在；当前候选未执行，不标PASS。
- 限制：首编E0599持续保留；本项精确验证范围见末节，Host未执行测试不标通过。
- 限制：0291 dirty WIP的15步API收据不是8a2e冻结Host/真实I/O运行；静态审查和源码测试数量也不构成执行通过。

## iface.change.managed-cross-provider-model-name-gap

已有registry按全局model逻辑名索引，本候选approved catalog同model跨provider会碰撞；用silent覆盖不能保证真实binding与策略归因。 install_managed_catalog通过seen(model)及已有models检查明确拒绝同名目录，所有校验先于修改；本批保持缺口并要求产品命名/路由方案另行验收。

- 状态归属：Kernel全局模型目录；PilotDeck P5兼容路由策略。
- 合同：`iface.contract.model`, `iface.contract.product-compat`, `iface.contract.assembly`；任务：`iface.P2.model-context`, `iface.P4.composition`, `iface.P5.cli`, `iface.P5.gateway`, `iface.P6.product`。
- 精确冻结源码：`kernel/host/src/transport/model_session/registry.rs:58`（`!seen.insert(binding.model.clone())`；blob `2183c3e8d499c34b539bf9c6c1512de209c6ce21`）；`kernel/host/src/transport/model_session/registry.rs:794`（`fn catalog_install_is_atomic_and_rejects_cross_provider_model_collision`；blob `2183c3e8d499c34b539bf9c6c1512de209c6ce21`）。
- 限制：明确拒绝是当前可观察行为，不是已实现同model跨provider兼容；P5保留未闭合，不称全产品完整。
- 限制：首编E0599持续保留；本项精确验证范围见末节，Host未执行测试不标通过。
- 限制：0291 dirty WIP的15步API收据不是8a2e冻结Host/真实I/O运行；静态审查和源码测试数量也不构成执行通过。

## iface.change.http-pull-identity-registration

新增pull API和same_source需要准确身份轴，不能把未发行候选加法称旧版已具备或Hub已发布。 env0.1.8→0.1.9，types0.1.10→0.1.11，carrier0.1.8/model0.1.7保持；33 types/8 env消费者精确钉和schemas更新，API2119+79=2198，稳定2032前缀保留。

- 状态归属：Kernel活动契约身份、schemas和消费者依赖登记。
- 合同：`iface.contract.packaging`, `iface.contract.assembly`, `iface.contract.model`, `iface.contract.auth`；任务：`iface.P0.sources`, `iface.P0.catalog`, `iface.P2.embedding`, `iface.P6.release`。
- 精确冻结源码：`products/iface-identity/env/mint.toml:7`（`version = "0.1.9"`；blob `942a804ef434b66cd3115007b94cb37b2b1eedce`）；`products/iface-identity/types/mint.toml:7`（`version = "0.1.11"`；blob `5e63632f72d7acfe61d280cb1e4d08e1dda4f521`）；`kernel/iface/api-baseline.txt:2109`（`HttpPullTransport`；blob `77a4dc2e885edb9e17db934ae2ee242c62e53dfc`）。
- 限制：15步原收据实际0291 dirty WIP；01schema写操作false保留，其余只读输入unchanged=true；静态登记/plan不代表8a2冻结验证或发布。
- 限制：首编E0599持续保留；本项精确验证范围见末节，Host未执行测试不标通过。
- 限制：0291 dirty WIP的15步API收据不是8a2e冻结Host/真实I/O运行；静态审查和源码测试数量也不构成执行通过。

## 源码、WIP实测、候选执行分别统计

- 冻结源码test attribute清点：HTTP scope13+cleanup9=22；trusted egress Host6+runtime8=14；managed外部组装3。清点本身不是PASS；目前runtime8已独立实测PASS，Host scope22/egress6/managed3仍因Host首编失败未运行。
- API15步有持久UTC/argv/前后SHA/log收据，真实HEAD0291 dirty WIP。01刷新schemas为source_unchanged=false；只读步骤保持true。默认close_scope Unsupported/same_source等consumer独立rustc运行PASS，只涉及公开iface，不是Host I/O。
- Host旧ExternalBackends/serve_resident兼容性只静态public-token对比，15步补入Host源scope，不代替真实Host编译。
- WIP snapshot 116个iface源与冻结8a2e相同，0不同；限源码适用性，不retag原run。
- API2198=前候选2119+79；ca0稳定2032前缀逐字保留。schema/version/plan不是Hub发布。
- 同model跨provider当前明确拒绝，保留P5兼容缺口；不能声称全产品完整。
- 优雅D96 close只在此HTTP权源中允许已claim且Running的generation继续派发/重试/drain；commit_guard仍拒close后的未claim排队工作。
- productionWired（PilotDeck正式入口）与jointlyVerified统一UNKNOWN；Kernel冻结生产路径源码不自动提升这两层。

| WIP实际检查 | 结果/类别 | 实测身份 |
| --- | --- | --- |
| `01-identity-refresh` | PASS / management | 0291 dirty WIP |
| `02-extract-api` | PASS / build | 0291 dirty WIP |
| `03-api-strict` | PASS / build | 0291 dirty WIP |
| `04-api-self-test` | PASS / build | 0291 dirty WIP |
| `05-identity-check` | PASS / build | 0291 dirty WIP |
| `06-versions-check` | PASS / build | 0291 dirty WIP |
| `07-versions-self-test` | PASS / build | 0291 dirty WIP |
| `08-publish-plan` | PASS / build | 0291 dirty WIP |
| `09-diff-check` | PASS / build | 0291 dirty WIP |
| `10-host-static-compatibility` | PASS / build | 0291 dirty WIP |
| `11-rustc-version` | PASS / build | 0291 dirty WIP |
| `12-iface-rlib` | PASS / build | 0291 dirty WIP |
| `13-consumer-compile` | PASS / build | 0291 dirty WIP |
| `14-consumer-run` | PASS / executed | 0291 dirty WIP |
| `15-host-source-scope-compatibility` | PASS / build | 0291 dirty WIP |

完整source SHA/blob/行、WIP原始依赖与contract/task映射见JSON。当前候选observedCandidateReceipts已收录5条首败/独立通过记录，notRunGroups保存Host未执行选择。本任务在交付后停写，主ledger由tableagent独占。

## 8a2e实际首编与独立受影响范围

- Host native-only生产库首编E0599 FAIL：`kernel/host/src/transport/server/assemble.rs:224`的`error.to_string()`要求iface Error实现Display/ToString，而冻结类型不具备。无Host产物，未执行external_backends6、trusted_egress6、http_scope22或whole model_session；default生产/测试与Host两feature Clippy也未运行。
- runtime async_dial8实际列出并8PASS、iface same_source1实际PASS，两二进制features[]及SHA前后相同；Host对应层不在独立范围内。
- Host/runtime/iface fmt PASS；runtime/iface all-targets Clippy -D warnings PASS。格式不编译Host，缩窄Clippy不包含Host，均不消除首编失败。

| 当前候选实际选择 | 结果/类别 | 范围 |
| --- | --- | --- |
| `host-production-native-only` | FAIL / build | 8a2e native-only生产库首编E0599 FAIL：server/assemble.rs:224把iface Error用to_string，但该类型未实现Display/ToString。没有Host产物或Host测试执行，实际features不可反填，仅保留--no-default-features请求。 |
| `runtime-async-dial` | PASS / executed | runtime async_dial精确列出8个并执行8PASS；二进制features[]/SHA前后一致。只覆盖runtime拨号层，不包括Host trusted_egress6/http_scope22。 |
| `iface-same-source` | PASS / executed | iface same_source1实际PASS；不证明close_scope/Host permit/pool生产行为。 |
| `fmt-host-runtime-iface` | PASS / build | Host/runtime/iface格式检查PASS；不编译Host代码，不能消除E0599。 |
| `clippy-runtime-iface` | PASS / build | 独立runtime/iface all-targets -D warnings PASS，未包含Host；不代替Host default/native Clippy或Host生产编译。 |

五条原receipt、日志、UTC、argv、源commit、features与SHA在JSON。WIP15步仍真实0291dirty身份不回标。静态Astra/source review与实际执行继续分开；生产组装联合验收UNKNOWN。
