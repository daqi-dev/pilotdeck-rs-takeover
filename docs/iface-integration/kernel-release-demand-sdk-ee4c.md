# 第六批：HTTP错误边界、原作业释放、消费信用与SDK观察

第六代码证据批已由root发布至三表，[实际回读](../../_tmp/20261115-iface-sixth-preparation/sync.json)为 `PASS_READBACK`：13合同／588任务及追踪／631运行，共1232行。该批账本SHA256为 `7ccb4e75caa54970231e7bed28f1ee3b6744b483d5b612efd7c8d0d95c99f32b`。管理回读确认数据与关联匹配，不表示产品已经完成正式Kernel组装、全部能力注入、联合验收、发行或默认切换。

源码范围从 `5d6d4569628625ec8f251beb13f5a9f2ab0c9477` 逐提交至 `ee4c6471652adc717d2aa8c116456d1492d6b849`，11个冻结提交登记为8项Kernel变化（74→82）。同时登记PD `d9aac452c72441f09ff7bc27cc4272a5ac55d580` 的4项有限产品边界变化（13→17），实际Mac运行仍是dirty2ac输入／依赖Kernel的HTTP closure为856，不回标clean d9aac执行。第29轮及之后、Mac c182／f616等后续代码证据不在本批。

源码审阅只读冻结Git对象；下文实际构建与运行来自独立验证收据，来源、编译、执行、产品接线和最终交付分开核实。[正式账本](../../state/iface-integration.json)保存完整事实与运行关联；新188条compact记录只引用完整原receipt的path与SHA。旧443条运行事实及投影、463条上游处置和合同审计不变，26项交付与旧127项分母独立。

[源码清单](../../_tmp/20261115-iface-sixth-preparation/evidence/kernel-trace/sources.json)保存216个冻结文件版本的commit、blob、完整SHA与快照，[11份提交patch](../../_tmp/20261115-iface-sixth-kernel-trace/commits/)保留逐提交差异。下列66个符号/行号均来自对应Git对象，已[逐项核验](../../_tmp/20261115-iface-sixth-preparation/kernel-source-attachment-audit.json)，不是live checkout行号。[独立语义复审](../../_tmp/20261115-iface-projection-review/sixth-semantic-review-v1.json)及[已更正说明](../../_tmp/20261115-iface-sixth-preparation/SIXTH-REVIEW-NOTES-v3.md)说明验收身份和限制。

## 逐提交归因

| 提交 | 本次语义 |
|---|---|
| `be4dcf561df96c3b38c624d0387a9f7d5bdbc952` | test(host): make remote protocol fixture execution explicit |
| `856305e41a0343e15b050c90c795b4f00ebf6046` | test(host): make TLS slow-consumer fixture reach real backpressure |
| `e7e8a9afa925981d2806dae143e8ab0db95274a3` | fix(host): reap owned child while confirming process group cleanup |
| `39e15b823dcb8cbdd1df3b9d0c70d457fb052252` | style(host): align owned child cleanup with lint and repository format |
| `ac59d9630dfbb2c845d874a2f19c89d187562d9f` | feat(iface): distinguish HTTP body limits and network timeouts |
| `1595f1b04964549ac131f7781281ef172e786749` | test(host): verify package install under the actual Wasm feature |
| `1fd8a7ae7562ac7fb51c51ea5813f0fc7e9075db` | feat(model): account for late local resource release independently |
| `1104fc1ddf730bf9a268706be43552f47063f3b8` | fix(host): preserve overflow event ownership with compact error |
| `1ece2f02d230dc727123788b4add1ce7c7ab4ffd` | feat(model): negotiate explicit consumer delivery credit |
| `d999f97be46a67be0971ddcd66e456d4e8df9e03` | feat(sdk): negotiate local model resource release observations |
| `ee4c6471652adc717d2aa8c116456d1492d6b849` | style(sdk): format model local release integration |

39e是owned-child代码的lint/格式收敛；1104将overflow返回事件装箱保所有权并清理借用；d999在delivery/http_scope仅去无谓借用，其业务新增是SDK；ee4c仅SDK五文件格式。不得把后续格式提交算成新的业务能力或把旧失败改成通过。

## 责任、接口与实际接线

### 1. remote fixture显式local后端并保留真实启动失败

原remote fixture让默认sandbox前提遮蔽协议验证，stderr被吞且仅等待注册超时。空账本Require也被错误预期为通过。

**接口或要求：**仅remote协议用例明确--backend local；等待观察子进程自然退出；RAII kill/wait；空账本拒绝码必须为COMPONENT_GATE_NO_KERNEL_COMPONENTS。

**源码已接入：**修改tests文件；exec-host由测试显式选择local，无自动生产回退。

**责任：**Kernel：保持生产sandbox默认与拒绝不变；为测试子进程建立回收责任。PilotDeck：不能将local协议PASS充作OS隔离或sandbox可用证据。

未完成或解释边界：

- 原三项remote失败仍保留，后续运行另登记。
- 原失败未观察到的cause不能由独立bwrap诊断反填。

维护者复核入口：

- `be4dcf56:kernel/host/src/transport/service/tests/mod.rs:593` — [struct ExecHostChild](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/be4dcf561df96c3b38c624d0387a9f7d5bdbc952/kernel/host/src/transport/service/tests/mod.rs#L593)；blob `d5dd93e8bae9e490e0cdd65ed5136cf4d97d6ca5`，SHA256 `a56ad9c0c86f1528af12d2e05a26db33f460faa0a735199556289950d5dee10b`。
- `be4dcf56:kernel/host/src/transport/service/tests/mod.rs:628` — [.arg("--backend")](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/be4dcf561df96c3b38c624d0387a9f7d5bdbc952/kernel/host/src/transport/service/tests/mod.rs#L628)；blob `d5dd93e8bae9e490e0cdd65ed5136cf4d97d6ca5`，SHA256 `a56ad9c0c86f1528af12d2e05a26db33f460faa0a735199556289950d5dee10b`。
- `be4dcf56:kernel/host/src/transport/service/tests/mod.rs:652` — [child.try_wait()](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/be4dcf561df96c3b38c624d0387a9f7d5bdbc952/kernel/host/src/transport/service/tests/mod.rs#L652)；blob `d5dd93e8bae9e490e0cdd65ed5136cf4d97d6ca5`，SHA256 `a56ad9c0c86f1528af12d2e05a26db33f460faa0a735199556289950d5dee10b`。
- `be4dcf56:kernel/host/src/startup/tool_env/tests.rs:220` — [COMPONENT_GATE_NO_KERNEL_COMPONENTS](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/be4dcf561df96c3b38c624d0387a9f7d5bdbc952/kernel/host/src/startup/tool_env/tests.rs#L220)；blob `24f217f829611ce720153a213643a6a8a9da58ea`，SHA256 `fe51e8bd803871c14e62b0ef126f4768782792b4d17862cef9e05d9701183dc0`。
- `be4dcf56:kernel/host/src/transport/service/tests/remote.rs:4` — [本组显式使用](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/be4dcf561df96c3b38c624d0387a9f7d5bdbc952/kernel/host/src/transport/service/tests/remote.rs#L4)；blob `79b143b7a13a10de39cc77a535c53f3903edffbc`，SHA256 `68a6f4166de5788ae4d2c06ca7b4f96f1cf0aa7b2671f798a75d797e3b60301a`。

### 2. TLS慢消费者fixture使真实socket达到背压并验证其他连接

原元数据流可被socket缓冲吸收，仅写循环停止不证明触达背压。

**接口或要求：**dev socket2双端请求4KiB并核实实际<=64KiB；首帧flush闸门；放行后写超时量级；最终EOF/reset且读到真实事件元数据。

**源码已接入：**真mTLS/TlsDuplex/serve_stream；首帧暂停时另一连接真实session.get成功；随后继续写并检查有界收尾。

**责任：**Kernel：只配置测试socket和同步首帧，保持生产stream写超时和终态逻辑。PilotDeck：联合验收另核产品consumer credit，不能由TLS慢消费者测试代替。

未完成或解释边界：

- 此提交无生产TLS协议修复。
- 测试同步闸门的等待不能冒充实际背压耗时；源码使用after_resume判断。

维护者复核入口：

- `856305e4:kernel/host/src/transport/tls/tests/model_stream.rs:115` — [struct FirstFrameGate](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/856305e41a0343e15b050c90c795b4f00ebf6046/kernel/host/src/transport/tls/tests/model_stream.rs#L115)；blob `23a31cf32df28c8320d262bc6bab07c0e3ab0c7d`，SHA256 `b2187bbe5f8f8568b3cb13650c812b61c3f4b8334beb6eaabbc275e956860c9e`。
- `856305e4:kernel/host/src/transport/tls/tests/model_stream.rs:181` — [const SOCKET_BUFFER](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/856305e41a0343e15b050c90c795b4f00ebf6046/kernel/host/src/transport/tls/tests/model_stream.rs#L181)；blob `23a31cf32df28c8320d262bc6bab07c0e3ab0c7d`，SHA256 `b2187bbe5f8f8568b3cb13650c812b61c3f4b8334beb6eaabbc275e956860c9e`。
- `856305e4:kernel/host/src/transport/tls/tests/model_stream.rs:285` — [let after_resume](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/856305e41a0343e15b050c90c795b4f00ebf6046/kernel/host/src/transport/tls/tests/model_stream.rs#L285)；blob `23a31cf32df28c8320d262bc6bab07c0e3ab0c7d`，SHA256 `b2187bbe5f8f8568b3cb13650c812b61c3f4b8334beb6eaabbc275e956860c9e`。
- `856305e4:kernel/host/src/transport/tls/tests/model_stream.rs:343` — [ended == "eof"](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/856305e41a0343e15b050c90c795b4f00ebf6046/kernel/host/src/transport/tls/tests/model_stream.rs#L343)；blob `23a31cf32df28c8320d262bc6bab07c0e3ab0c7d`，SHA256 `b2187bbe5f8f8568b3cb13650c812b61c3f4b8334beb6eaabbc275e956860c9e`。
- `856305e4:kernel/host/Cargo.toml:153` — [socket2 =](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/856305e41a0343e15b050c90c795b4f00ebf6046/kernel/host/Cargo.toml#L153)；blob `c8376bdd31f396cc59ef3a7c4e3dfc25ffa49f79`，SHA256 `dfeb22fd7bc88622cc43297f231f96ea9931dd43a14cc888c2e8289e27b46ecd`。

### 3. 本地进程组回收持有直属Child并先撤销未来信号权限

只杀组后观察组是否存在，可能未wait直属leader；释放leader身份后继续按裸PGID发信号会引入复用风险。

**接口或要求：**retire→组KILL一次→kill leader→try_wait leader且确认组空；未证实返回false并记录unconfirmed，不恢复重试权限。

**源码已接入：**生产reap签名从PGID改为&mut Child，stop_child_group及command错误路径实际接入。

**责任：**Kernel：持有Child直到回收；command stdin写失败走stop_child_group；39e仅lint/fmt收敛。PilotDeck：不将本地进程组回收当HTTP scope或provider停止回执。

未完成或解释边界：

- reap_all仍为原登记组批量观察路径，本提交没有为它新增持有Child的回执。
- non-Unix仍只回收直接子进程；stop_child_group的兜底wait未改为一个新的跨平台有界协议。
- 两个假动作测试验证顺序/不复权，另一个真实sleep测试验证leader收集；不能都称真实树测试。

维护者复核入口：

- `e7e8a9af:kernel/host/src/transport/service/local_exec.rs:130` — [trait ReapActions](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/e7e8a9afa925981d2806dae143e8ab0db95274a3/kernel/host/src/transport/service/local_exec.rs#L130)；blob `b32e7031ca9fcf4a7f5165d568b4ce1194bbcea4`，SHA256 `be09d6f5b7579e9366a0b846e615e502d858e2c2ba41a08236e21e407faddd2b`。
- `39e15b82:kernel/host/src/transport/service/local_exec.rs:143` — [fn reap_with_actions](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/39e15b823dcb8cbdd1df3b9d0c70d457fb052252/kernel/host/src/transport/service/local_exec.rs#L143)；blob `7c479d752379467108dccafcf16c32a3dd33957c`，SHA256 `580bb75536a8618de5d2926f3f33cfa2fd4c77db36b0c0cb7d281b4a5ca381cb`。
- `39e15b82:kernel/host/src/transport/service/local_exec.rs:378` — [pub(crate) fn reap(child](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/39e15b823dcb8cbdd1df3b9d0c70d457fb052252/kernel/host/src/transport/service/local_exec.rs#L378)；blob `7c479d752379467108dccafcf16c32a3dd33957c`，SHA256 `580bb75536a8618de5d2926f3f33cfa2fd4c77db36b0c0cb7d281b4a5ca381cb`。
- `e7e8a9af:kernel/host/src/transport/service/command.rs:83` — [local_exec::stop_child_group](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/e7e8a9afa925981d2806dae143e8ab0db95274a3/kernel/host/src/transport/service/command.rs#L83)；blob `442569d6f20b3c76cfd6406aace24386a94f9755`，SHA256 `3b1d3578bef4d7d25059f5477e9d8c4d4b3fcc74d5e206c8f85505da31507f7a`。
- `39e15b82:kernel/host/src/transport/service/local_exec/tests.rs:290` — [fn collected_leader_with_present_group](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/39e15b823dcb8cbdd1df3b9d0c70d457fb052252/kernel/host/src/transport/service/local_exec/tests.rs#L290)；blob `d2c8b6d7f4b067d12e6d502a2eda160440716e77`，SHA256 `76a613ec54d9f868191bd6258a1591b2fea20b29210aa51ecf8534bc4d11dda9`。
- `39e15b82:kernel/host/src/transport/service/local_exec/tests.rs:314` — [fn reap_collects_a_direct_leader](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/39e15b823dcb8cbdd1df3b9d0c70d457fb052252/kernel/host/src/transport/service/local_exec/tests.rs#L314)；blob `d2c8b6d7f4b067d12e6d502a2eda160440716e77`，SHA256 `76a613ec54d9f868191bd6258a1591b2fea20b29210aa51ecf8534bc4d11dda9`。

### 4. env0.1.11区分正文上限与通用资源和网络超时

PD必须区分明确正文大小限制、一般资源不足及可信期限；不能从错误文案猜ETIMEDOUT或把DNS/TCP超时降成scope期限。

**接口或要求：**新增ResponseBodyTooLarge；TimedOut保持Network并由io::ErrorKind保ETIMEDOUT；Deadline仅可信执行期限；更新env身份schema/API/pin。

**源码已接入：**trusted_egress的DNS/TCP错误映射已接生产路径；TooLarge是公开分类，实际正文触发及产品消费需另核。

**责任：**Kernel：公开类型并修trusted egress map；此提交不凭空为正文传输制造超限失败。PilotDeck：真实transport在明确配置字节上限触发时产生TooLarge；产品保精确V8文本限制，不把所有Resource变成StringTooLong。

未完成或解释边界：

- enum分类与schema声明不证明PD全链错误映射已接好。
- 底层不明错误的detail含timeout/undici文字时仍无已知code；不制造网络码或retryable。
- 身份版本不代表Hub已发布或真实安装成功。

维护者复核入口：

- `ac59d963:kernel/iface/src/env/http_pull.rs:182` — [pub enum HttpFailureKind](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/ac59d9630dfbb2c845d874a2f19c89d187562d9f/kernel/iface/src/env/http_pull.rs#L182)；blob `019ba321f5b3178e69f24ef80c399a0104e0895d`，SHA256 `aa01fd28a38783cf868fccdf161b2a6f10ef71a555fa8bb8ce235632b1d2ae76`。
- `ac59d963:kernel/iface/src/env/http_pull.rs:197` — [ResponseBodyTooLarge,](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/ac59d9630dfbb2c845d874a2f19c89d187562d9f/kernel/iface/src/env/http_pull.rs#L197)；blob `019ba321f5b3178e69f24ef80c399a0104e0895d`，SHA256 `aa01fd28a38783cf868fccdf161b2a6f10ef71a555fa8bb8ce235632b1d2ae76`。
- `ac59d963:kernel/host/src/startup/trusted_egress.rs:477` — [fn map_egress](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/ac59d9630dfbb2c845d874a2f19c89d187562d9f/kernel/host/src/startup/trusted_egress.rs#L477)；blob `489bec30a17820ff65dd366cd66df6f02b55d43b`，SHA256 `0a8e9d4e910303b2cdf196833caba2d101cf8cca3f88ffa95488ded08307d4f9`。
- `ac59d963:kernel/host/src/startup/trusted_egress_tests.rs:179` — [fn dns_and_tcp_io_timeouts](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/ac59d9630dfbb2c845d874a2f19c89d187562d9f/kernel/host/src/startup/trusted_egress_tests.rs#L179)；blob `49c2122d85696dcf9c141fc68195eab7fb352f0b`，SHA256 `01eed62f368d0313dc58ae16bd5bad8ed885b62d6285e8c70166d6aebd087925`。
- `ac59d963:kernel/host/src/startup/trusted_egress_tests.rs:202` — [fn timeout_words_and_undici](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/ac59d9630dfbb2c845d874a2f19c89d187562d9f/kernel/host/src/startup/trusted_egress_tests.rs#L202)；blob `49c2122d85696dcf9c141fc68195eab7fb352f0b`，SHA256 `01eed62f368d0313dc58ae16bd5bad8ed885b62d6285e8c70166d6aebd087925`。
- `ac59d963:products/iface-identity/env/mint.toml:7` — [version = "0.1.11"](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/ac59d9630dfbb2c845d874a2f19c89d187562d9f/products/iface-identity/env/mint.toml#L7)；blob `ecca9ba4e23ac782dcfa6b090da993d6905e6e59`，SHA256 `1cf7f40d7bb581fafc177931ca85e0933414eb1b088efa770bec5c5597b1e28a`。

### 5. package install分别验证native拒绝与Wasm成功真实范围

未启wasm-host的native构建不应运行期望Wasm成功的测试，也不能跳过真实apply后的拒绝原子性。

**接口或要求：**wasm-host分支保真实装载/发布/receipt；native分支真正plan/apply/files verify后明确FeatureDisabled并保持Unknown且零新能力/零成功receipt。

**源码已接入：**native两个测试用FakeProvider实际apply一次，验证旧binding Arc原样保留且无pending发布；有feature才选择两个成功case。

**责任：**Kernel：按实际feature编译对应测试；生产控制器不改，不伪造从未执行。PilotDeck：核对Mint协议兼容与真实制品；native拒绝不是正式成功安装。

未完成或解释边界：

- FakeProvider不是正式Mint服务。
- native测试以controller-test-component非Wasm字节确保在feature gate拒绝，不能算Wasm guest执行。
- wasm-host启用仍须核实guest产物实际存在/执行；内部SKIP和legacy Mint旧协议项不得算actual成功安装。

维护者复核入口：

- `1595f1b0:kernel/host/src/transport/service/package_install/tests.rs:38` — [mod native;](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1595f1b04964549ac131f7781281ef172e786749/kernel/host/src/transport/service/package_install/tests.rs#L38)；blob `4395b3603a7b370939299dff5c46c0a8cfc01bb4`，SHA256 `6e5bdaa2ea10ebcbbebaf00afb7690cc4cc153353ab155485704969cd1dd932f`。
- `1595f1b0:kernel/host/src/transport/service/package_install/tests.rs:520` — [fn successful_provider_publishes](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1595f1b04964549ac131f7781281ef172e786749/kernel/host/src/transport/service/package_install/tests.rs#L520)；blob `4395b3603a7b370939299dff5c46c0a8cfc01bb4`，SHA256 `6e5bdaa2ea10ebcbbebaf00afb7690cc4cc153353ab155485704969cd1dd932f`。
- `1595f1b0:kernel/host/src/transport/service/package_install/tests/native.rs:6` — [pub(super) fn assert_disabled_wasm](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1595f1b04964549ac131f7781281ef172e786749/kernel/host/src/transport/service/package_install/tests/native.rs#L6)；blob `f5a8f940a5d4ea3b760fb303a20e03530d1ee696`，SHA256 `69dd29999cd491ab8bf17438d9abd15cbf21f504eb789ded445520c27beeaf43`。
- `1595f1b0:kernel/host/src/transport/service/package_install/tests/native.rs:51` — [ToolOutcome::Unknown](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1595f1b04964549ac131f7781281ef172e786749/kernel/host/src/transport/service/package_install/tests/native.rs#L51)；blob `f5a8f940a5d4ea3b760fb303a20e03530d1ee696`，SHA256 `69dd29999cd491ab8bf17438d9abd15cbf21f504eb789ded445520c27beeaf43`。
- `1595f1b0:kernel/host/src/transport/service/package_install/tests/dependency_graph.rs:242` — [fn healthy_dependency_apply_rejects](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1595f1b04964549ac131f7781281ef172e786749/kernel/host/src/transport/service/package_install/tests/dependency_graph.rs#L242)；blob `c26b99ed3b29dfa107bdedd29d05dd94982d0d19`，SHA256 `f02f579d1bd1835c7cccd4bd3ea5af0f297a4d107368ca3c6db1db6b0e2ad8a6`。

### 6. model0.1.8以原owner正向回执独立结算本地资源

业务Completed/Failed/Unknown、远端Stopped或取消动作均不能证明原generation本地资源已释放。

**接口或要求：**opt-in producer与observer；原reader/Arc backend/commit permit/context/同源cancel/input/revision/response精确匹配；local held独立单调false；通用注入不接受receipt。

**源码已接入：**claim时设置local_resource_held；reader专用apply验证后置false，Unknown仍Unknown；正常事件路径拒绝该事实；public wire仅job_id及常规头且无终态。

**责任：**Kernel：保原owner到真实释放，纳共同并发和shutdown drain；receipt绕过满ingress并保存真实事实先于wire publication。PilotDeck：仅在真实scope、lease、池、helper全部释放后，用原context生成事件；不得仅据abort ack/业务终态报告释放。

未完成或解释边界：

- ModelLocalResourceRelease::new可公开构造，构造不授予authority；真正信任来源是原reader与原permit核验。
- Kernel冻结树内生产backend未见new调用，所见构造在iface/host测试；此范围不能证明PD真实backend发出回执。
- 本地释放不证明backend_resources_released/session closed/远端停止/退款/账单停止。
- 序号耗尽或发布失败仍保存真实记账，观察者可能看不到wire事件，不能伪造cursor。
- 1104是overflow Err装箱与closure lint，不是新释放机制。

维护者复核入口：

- `1fd8a7ae:kernel/iface/src/model_session/event.rs:10` — [pub struct ModelLocalResourceRelease](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1fd8a7ae7562ac7fb51c51ea5813f0fc7e9075db/kernel/iface/src/model_session/event.rs#L10)；blob `455bd66afb30845ce64ce2109c1288226e484be7`，SHA256 `04a0d740b7b53264d983637cc1f8dd8e36584498b5a9959d518519ec2dee532e`。
- `1fd8a7ae:kernel/iface/src/model_session/profile.rs:6` — [pub const MODEL_LOCAL_RESOURCE_FACTS_PROFILE_V1](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1fd8a7ae7562ac7fb51c51ea5813f0fc7e9075db/kernel/iface/src/model_session/profile.rs#L6)；blob `1444bf9b2fe605f1fc511f66b56760459081e341`，SHA256 `e939bce350d36af1609c62d8490d9e7f778e21908fa25972c4557a2f667967ed`。
- `1fd8a7ae:kernel/host/src/transport/model_session/execution/commit_guard.rs:244` — [job.local_resource_held = true](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1fd8a7ae7562ac7fb51c51ea5813f0fc7e9075db/kernel/host/src/transport/model_session/execution/commit_guard.rs#L244)；blob `8c1a307a003e48d37fa4e6bfa044ed3bae172634`，SHA256 `89210dcb40760c6873c868e65aed8f95e665ead54382cd0f09498ffff466159e`。
- `1fd8a7ae:kernel/host/src/transport/model_session/execution.rs:621` — [pub(crate) fn observe_local_release](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1fd8a7ae7562ac7fb51c51ea5813f0fc7e9075db/kernel/host/src/transport/model_session/execution.rs#L621)；blob `22bf07efb38782d4e987aefbb344357cc781e4a3`，SHA256 `f931835ed397d100cc6fe425e3ec72239d95d35522a5d6de73bc4618d1533f73`。
- `1fd8a7ae:kernel/host/src/transport/model_session/execution/http_scope.rs:27` — [pub(super) fn matches_release_owner](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1fd8a7ae7562ac7fb51c51ea5813f0fc7e9075db/kernel/host/src/transport/model_session/execution/http_scope.rs#L27)；blob `c7e20cfc4122e273280c77e655f11d07b356aa82`，SHA256 `51cd9793802a69789c053b3b33527385fc2494969022de6bc279445b646b4f79`。
- `1fd8a7ae:kernel/host/src/transport/model_session/registry.rs:329` — [pub(crate) fn apply_local_release_from_reader](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1fd8a7ae7562ac7fb51c51ea5813f0fc7e9075db/kernel/host/src/transport/model_session/registry.rs#L329)；blob `1dd89ee3ad4e04ed3d4f4f02501bb963ba19ea1e`，SHA256 `b56540d5630f779b7bcf4bad82251bca52b7d21c6de7be68d16d8537c8b6c7eb`。
- `1fd8a7ae:kernel/host/src/transport/model_session/event_driver.rs:164` — [registry::apply_local_release_from_reader](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1fd8a7ae7562ac7fb51c51ea5813f0fc7e9075db/kernel/host/src/transport/model_session/event_driver.rs#L164)；blob `2d965491ae6189f6a1120b02da5d2efb3bae68f4`，SHA256 `902dcdcfa4244ed475cc472049a0a1767884d1d7db077df32d1020c521d03fcf`。
- `1fd8a7ae:kernel/host/src/transport/model.rs:50` — [job.local_resource_held](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1fd8a7ae7562ac7fb51c51ea5813f0fc7e9075db/kernel/host/src/transport/model.rs#L50)；blob `659356f4554c82fb91ab549b95efa9aabb5c7b4f`，SHA256 `8afa4cd0222de214fe80fb3cd7e7033fc0f08aecb65d5a8869aea51f040dda31`。
- `1fd8a7ae:kernel/host/src/transport/model_session/access.rs:359` — [profile must be negotiated by the observer](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1fd8a7ae7562ac7fb51c51ea5813f0fc7e9075db/kernel/host/src/transport/model_session/access.rs#L359)；blob `fca3e9a44594536a75e51e668ecb480e474ce7b2`，SHA256 `56e8ede958eb95c165e213e25888dabaafb42e4ea0f2a3d8a9c490eabf7f7564`。
- `1fd8a7ae:kernel/host/src/transport/model_session/execution/local_release_tests.rs:203` — [fn old_settled_receipt](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1fd8a7ae7562ac7fb51c51ea5813f0fc7e9075db/kernel/host/src/transport/model_session/execution/local_release_tests.rs#L203)；blob `162cc73f9515f51eb0426a9224a056dbc6e4a6a1`，SHA256 `576e71f21f97cb3c9f1a85b079fc25d76f08e9e66aa18ea29389c3b72337588f`。
- `1104fc1d:kernel/host/src/transport/model_session/event_driver.rs:55` — [fn enqueue](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1104fc1ddf730bf9a268706be43552f47063f3b8/kernel/host/src/transport/model_session/event_driver.rs#L55)；blob `f42a6e2ef73da995e54cc99062e149c6ca272305`，SHA256 `27e1245251e74034adbc2196b6ef7e5f19ab1713ea885ba6e0fde6076b2577d0`。
- `1fd8a7ae:products/iface-identity/model/mint.toml:7` — [version = "0.1.8"](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1fd8a7ae7562ac7fb51c51ea5813f0fc7e9075db/products/iface-identity/model/mint.toml#L7)；blob `9abfa6a68ad6143bb75a8641d29c2e0f4fd11db8`，SHA256 `9d5fb1231558f8a8cc815c22f2d0e60628a8d5351cb0ba6712d9329df49e0abc`。

### 7. model0.1.9将consumer推进设为独立写操作且初始零信用

读取事件历史不应启动supplier/HTTP或隐式确认；每份source交付需可追溯到原generation的一次显式consumer信用。

**接口或要求：**Request模式且显式request/event/demand三profile；commit只装关闭门且不等信用；previous=null首次推进、后续精确公共source seq；重复幂等/一次一pending。

**源码已接入：**Host dispatch选demand专用commit；Initial只有null advance变Running；Structured发布转Waiting，读pages/next_event不advance；cleanup事实独立不耗/不发信用。

**责任：**Kernel：新增delivery_advance写方法、epoch/policy/source/permit复查；锁外回调；host seq映射backend seq；错序/无credit/留存gap/发布失败停止交付保未知。PilotDeck：生产backend覆盖两个默认Unsupported方法；consumer真正调用advance并严格按已交付Structured source确认；首次前不拿supplier/不pollHTTP。

未完成或解释边界：

- 冻结Kernel生产实现搜索仅trait默认与DemandBackend测试，不能说已有生产supplier门或PD消费全链接线。
- 首次显式信用不意味着预先发一份默认信用；提交本身只装门。
- 仅unlocked resident dispatch路径支持；旧同步route明确Unsupported；未协商后端保持旧eager路径。
- terminal与accepted callback会合、late usage不改replacement credit、held释放不回退均已在1ece；d999仅相关无谓借用lint。
- 首次原因仲裁、Interrupted parity、真实resident consumer-demand联验仍未闭；_tmp产品draft不算实现。

维护者复核入口：

- `1ece2f02:kernel/iface/src/model_session/profile.rs:6` — [pub const MODEL_CONSUMER_DEMAND_PROFILE_V1](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/iface/src/model_session/profile.rs#L6)；blob `d843d29a4ed77b0695060a10db357d7ce4ce7d2b`，SHA256 `738f4d36f63cb20a0a78b9680b7cf84669d3c10d7db5fe173c44d3e31e283cac`。
- `1ece2f02:kernel/iface/src/model_session/profile.rs:43` — [consumer demand requires request and event profiles](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/iface/src/model_session/profile.rs#L43)；blob `d843d29a4ed77b0695060a10db357d7ce4ce7d2b`，SHA256 `738f4d36f63cb20a0a78b9680b7cf84669d3c10d7db5fe173c44d3e31e283cac`。
- `1ece2f02:kernel/iface/src/model_session/operations.rs:197` — [fn commit_input_demand_scoped](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/iface/src/model_session/operations.rs#L197)；blob `cdae2f4ff9eab7b83058214e53de032b4bfdc8a9`，SHA256 `10d25f2ac6d3640470d017bfb38fcfe24f3602b19ccde870b8624adc6dbfc9c5`。
- `1ece2f02:kernel/iface/src/model_session/operations.rs:210` — [fn advance_delivery](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/iface/src/model_session/operations.rs#L210)；blob `cdae2f4ff9eab7b83058214e53de032b4bfdc8a9`，SHA256 `10d25f2ac6d3640470d017bfb38fcfe24f3602b19ccde870b8624adc6dbfc9c5`。
- `1ece2f02:kernel/host/src/transport/model_session/execution.rs:87` — [pub(crate) fn dispatch](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/host/src/transport/model_session/execution.rs#L87)；blob `d415e5b8c3a906403572e2780e962bbc67fc6b77`，SHA256 `f10c6fce972bcbdd0ab4974526adb6c67faba8c3a95a0d3fca1f7be8dd51dc47`。
- `1ece2f02:kernel/host/src/transport/model_session/execution/delivery.rs:34` — [pub(crate) fn new(enabled](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/host/src/transport/model_session/execution/delivery.rs#L34)；blob `d53f8f327f661b6f68adb851fa6ed673fd9f1cc1`，SHA256 `8836e0821b1491984d3451b575c7316726d3aac28d1de44beef69731453980b9`。
- `1ece2f02:kernel/host/src/transport/model_session/execution/delivery.rs:51` — [fn begin(](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/host/src/transport/model_session/execution/delivery.rs#L51)；blob `d53f8f327f661b6f68adb851fa6ed673fd9f1cc1`，SHA256 `8836e0821b1491984d3451b575c7316726d3aac28d1de44beef69731453980b9`。
- `1ece2f02:kernel/host/src/transport/model_session/execution/delivery.rs:142` — [pub(in crate::transport::model_session) fn prepare](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/host/src/transport/model_session/execution/delivery.rs#L142)；blob `d53f8f327f661b6f68adb851fa6ed673fd9f1cc1`，SHA256 `8836e0821b1491984d3451b575c7316726d3aac28d1de44beef69731453980b9`。
- `1ece2f02:kernel/host/src/transport/model_session/execution/delivery.rs:242` — [fn recheck(](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/host/src/transport/model_session/execution/delivery.rs#L242)；blob `d53f8f327f661b6f68adb851fa6ed673fd9f1cc1`，SHA256 `8836e0821b1491984d3451b575c7316726d3aac28d1de44beef69731453980b9`。
- `1ece2f02:kernel/host/src/transport/model_session/execution/delivery.rs:307` — [pub(crate) fn execute(](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/host/src/transport/model_session/execution/delivery.rs#L307)；blob `d53f8f327f661b6f68adb851fa6ed673fd9f1cc1`，SHA256 `8836e0821b1491984d3451b575c7316726d3aac28d1de44beef69731453980b9`。
- `1ece2f02:kernel/host/src/transport/model_session/execution/delivery.rs:450` — [pub(crate) fn retain_cleanup_fact](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/host/src/transport/model_session/execution/delivery.rs#L450)；blob `d53f8f327f661b6f68adb851fa6ed673fd9f1cc1`，SHA256 `8836e0821b1491984d3451b575c7316726d3aac28d1de44beef69731453980b9`。
- `1ece2f02:kernel/host/src/transport/model_session/execution/delivery.rs:552` — [pub(crate) fn validate_event](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/host/src/transport/model_session/execution/delivery.rs#L552)；blob `d53f8f327f661b6f68adb851fa6ed673fd9f1cc1`，SHA256 `8836e0821b1491984d3451b575c7316726d3aac28d1de44beef69731453980b9`。
- `1ece2f02:kernel/host/src/transport/model_session/route.rs:215` — [delivery advance requires the unlocked](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/host/src/transport/model_session/route.rs#L215)；blob `cb913cd2148c54909be38ea76d4d2f300d30e50f`，SHA256 `14fc3bb78c375d010c34c005cb2456cbac2466ecc68019c6f45153c98c2140c1`。
- `1ece2f02:kernel/scheduler-protocol/src/model_session_resources.rs:71` — [pub struct DeliveryAdvanceRequest](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/scheduler-protocol/src/model_session_resources.rs#L71)；blob `b6b9d976c778d8531fc7aeadfb42c5bc58e43294`，SHA256 `9af06e3087d76d6f9b029908ecb06cd9c2395815d357169081bac50ce93f8c4c`。
- `1ece2f02:kernel/host/src/transport/model_session/execution/delivery/tests.rs:167` — [fn initial_credit_is_explicit](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/host/src/transport/model_session/execution/delivery/tests.rs#L167)；blob `c5a5f0d1bbf249abf65a1c26c4c54278b96b81ec`，SHA256 `479aec19dd77ce9773aad3f02c3a10fe516183c83b8a5d56d408cfdefa0fb232`。
- `1ece2f02:kernel/host/src/transport/model_session/execution/delivery/tests.rs:396` — [fn completed_between_pending_and_wake](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/host/src/transport/model_session/execution/delivery/tests.rs#L396)；blob `c5a5f0d1bbf249abf65a1c26c4c54278b96b81ec`，SHA256 `479aec19dd77ce9773aad3f02c3a10fe516183c83b8a5d56d408cfdefa0fb232`。
- `1ece2f02:kernel/host/src/transport/model_session/execution/delivery/tests.rs:723` — [fn observed_local_release_is_not_reacquired](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/kernel/host/src/transport/model_session/execution/delivery/tests.rs#L723)；blob `c5a5f0d1bbf249abf65a1c26c4c54278b96b81ec`，SHA256 `479aec19dd77ce9773aad3f02c3a10fe516183c83b8a5d56d408cfdefa0fb232`。
- `1ece2f02:products/iface-identity/model/mint.toml:7` — [version = "0.1.9"](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/1ece2f02d230dc727123788b4add1ce7c7ab4ffd/products/iface-identity/model/mint.toml#L7)；blob `704805d2ea41b0d0209fe5aa1bc66fd1be14be02`，SHA256 `2858761e270bcc77ff439afd9030b0d1d9ff7ff0520e4e7745e3b46636d5d3c2`。

### 8. SDK显式协商本地释放profile并保旧reader wire边界

生产者会话支持新事件不等于严格旧reader可以接收；主连接、stream连接和session profile需分别确认。

**接口或要求：**默认false；新start_with_local_resource_facts显式入口；StreamSource默认不支持；两个initialize回包确认feature，session_open确认profile；缺profile请求abort但释放仍unknown。

**源码已接入：**new/start_with_stream保持旧wire；opt-in才加feature/extensions；local_resources_released被sidecar转换器识别但不造delta/terminal；ee4c为5文件格式收敛。

**责任：**Kernel：提供显式builder/入口、双连接和session三处检查、无内容/无终态投影。PilotDeck：产品组合根主动选新入口并提供支持的reader；如要consumer-demand仍需另写显式推进。

未完成或解释边界：

- supports_local_resource_facts()标识配置意图，真实连接握手在open时仍须成功。
- SDK只添加local release协商，没有为sidecar接好consumer-demand；不把普通stream_next当advance。
- SDK丢弃该事实的sidecar内容投影，不代表替产品全部资源owner完成收敛或暴露独立审计API。
- ee4c已纳第六正式范围；SDK63及Clippy/fmt按各自实际版本入表，旧d999 SDKfmtFAIL仍保留。

维护者复核入口：

- `d999f97b:kernel/agent-host/src/exchange.rs:99` — [pub fn with_local_resource_facts](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/d999f97be46a67be0971ddcd66e456d4e8df9e03/kernel/agent-host/src/exchange.rs#L99)；blob `f126e1517efb14c54c83eb1626cb9f4859f10ee3`，SHA256 `d81933bacef79547e9b716455bd2328f926e684d30a1dacca76035ed464b3ca5`。
- `d999f97b:kernel/agent-host/src/exchange.rs:642` — [kernel did not negotiate local resource facts on main connection](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/d999f97be46a67be0971ddcd66e456d4e8df9e03/kernel/agent-host/src/exchange.rs#L642)；blob `f126e1517efb14c54c83eb1626cb9f4859f10ee3`，SHA256 `d81933bacef79547e9b716455bd2328f926e684d30a1dacca76035ed464b3ca5`。
- `d999f97b:kernel/agent-host/src/session.rs:92` — [pub fn start_with_local_resource_facts](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/d999f97be46a67be0971ddcd66e456d4e8df9e03/kernel/agent-host/src/session.rs#L92)；blob `395f5daba2db933a8b3c71a02cbfe0a6386f7b07`，SHA256 `da281d632cfc1b6c218ce0a30d8c208222e6187d925c7c559165b0945f25433c`。
- `d999f97b:kernel/agent-host/src/stream_source.rs:44` — [fn supports_local_resource_facts](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/d999f97be46a67be0971ddcd66e456d4e8df9e03/kernel/agent-host/src/stream_source.rs#L44)；blob `9bec2b45c4f785536bd2447a87f8ad589f302df6`，SHA256 `c028ecee4e6402fbea963cbfa413c397a221df912fa9fd6185ad796e26254efa`。
- `d999f97b:kernel/agent-host/src/stream_source.rs:266` — [kernel did not negotiate local resource facts on stream connection](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/d999f97be46a67be0971ddcd66e456d4e8df9e03/kernel/agent-host/src/stream_source.rs#L266)；blob `9bec2b45c4f785536bd2447a87f8ad589f302df6`，SHA256 `c028ecee4e6402fbea963cbfa413c397a221df912fa9fd6185ad796e26254efa`。
- `d999f97b:kernel/agent-host/src/exchange/model_stream/startup.rs:79` — [cleanup outcome is unknown](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/d999f97be46a67be0971ddcd66e456d4e8df9e03/kernel/agent-host/src/exchange/model_stream/startup.rs#L79)；blob `ea09b1586e1d2702750a4d79fdd3798f3534c234`，SHA256 `fb68b4500a5c7f8ca350a7cd261f160df5c00327620f00d871ca6db6e694b5f6`。
- `d999f97b:kernel/agent-host/src/exchange/model_stream.rs:563` — ["local_resources_released" => {}](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/d999f97be46a67be0971ddcd66e456d4e8df9e03/kernel/agent-host/src/exchange/model_stream.rs#L563)；blob `65dcf6c356180d984e13116f7e732e506c9df6b1`，SHA256 `4189023d388e8ad5fe2fc00bae9e6a8ea9d1cf7563c0c9a624ca222cb742b4ec`。
- `ee4c6471:kernel/agent-host/src/exchange.rs:99` — [pub fn with_local_resource_facts](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/ee4c6471652adc717d2aa8c116456d1492d6b849/kernel/agent-host/src/exchange.rs#L99)；blob `f5c1e9e55d6b63b89571c1a72b2be2f14ceade41`，SHA256 `f8d7293e629464fe07d5417e53e4e653a8d849f0d1b0cb10166251c35079d43a`。
- `ee4c6471:kernel/agent-host/src/session.rs:90` — [pub fn start_with_local_resource_facts](../../_tmp/20261115-iface-sixth-kernel-trace/frozen-source/ee4c6471652adc717d2aa8c116456d1492d6b849/kernel/agent-host/src/session.rs#L90)；blob `584975b73c23c09f298ba65fed4a696c26603bda`，SHA256 `b5b434435da918289e17593310c270b8a3a2e52ea0ec99e9650cbe05a06fe3b6`。

## 初始信用与本地释放的因果边界

Request输入commit完成准入并不启动consumer-demand driver。Host创建Initial状态，backend的demand commit只记录sealed input/context并返回；不能等待首份信用，否则会卡住原commit fence。首次消费明确调用`model.delivery_advance`且`previous_event_seq:null`，此时才给一次信用。收到一个Structured source事件后，Host记住公共序号与backend序号的唯一映射并进入Waiting；下一次显式推进确认该公共序号，再传原backend序号。单纯读event page、订阅流、next_event或重复同一确认均不增加信用。

取消或交付失败后的Usage/ToolCallCompleted可沿原代保存；LocalResourcesReleased走原reader专用路径，不需要consumer信用，也不能打开门。业务已完成但本地仍held的owner继续占并发及shutdown drain；真实释放只消除local held，Unknown不能变Completed，local released也不能推出remote stopped。

原reader与backend Arc身份、原permit、同源取消令牌和精确事件头是authority核验。公开release值的构造器只携带归属资料；构造成功不等于能从普通wire入口让Host释放资源。已经settled的旧代回执可结算旧owner，但不能改新代信用；当前匹配的后端与事件头还要通过专用registry检查。

## 已发布的实际验证与版本边界

以下全部按原receipt实际身份记载；不同版本、feature或重复定向命令不相加为distinct能力覆盖。原失败没有被后继通过覆盖。

| 实际版本／命令范围 | 原结果及适用边界 |
|---|---|
| [856定向](../../_tmp/20261115-iface-linux/kernel-856305e4-summary.json) | 25PASS；remote显式local不证明默认sandbox可用。TLS fixture实际达到背压，不改变原60s判据。 |
| [e7原回收机制](../../_tmp/20261115-iface-linux/kernel-e7e8a9af-summary.json)／[39后继](../../_tmp/20261115-iface-linux/kernel-39e15b82-summary.json) | e7实际36PASS，fmt/Clippy首败保留；host-lifecycle初始list预期3、实际9，未执行的原选择与补充实际9项分开。39完整Native libtest1790=1788PASS／2Wasm前置FAIL。 |
| [ac59错误分类](../../_tmp/20261115-iface-linux/kernel-ac59d963-summary.json) | 定向264PASS，API/identity/versions/fmt/Clippy通过；网络TimedOut断言不冒充公共DNS/TCP现场实验。 |
| [1595 feature范围](../../_tmp/20261115-iface-linux/kernel-1595f1b0-summary.json) | Native完整libtest1792PASS；当时无内部SKIP审计，不证明真实Mint链执行。default仅编译/list1827，运行0。 |
| [1fd8本地释放](../../_tmp/20261115-iface-linux/kernel-1fd8a7ae-summary.json) | 定向579PASS，Clippy3处首败；后继1104修正另记，不重标原结果。 |
| [WASI源码前置](../../_tmp/20261115-iface-linux/kernel-1fd8a7ae-prerequisites-summary.json)／[真实默认Wasm](../../_tmp/20261115-iface-linux/kernel-1fd8a7ae-default-wasm-summary.json) | 两个guest源码build PASS与后续默认配置20项实际PASS分开。Mac交叉编译libc和metadata重建不等于执行组件，原190缺crate／缺-lc及metadata汇编首败都保留。 |
| [1104矩阵](../../_tmp/20261115-iface-linux/kernel-1104fc1d-summary.json) | Native受影响278+7PASS；默认libtest1838含22内部SKIP，仅1816行为完成；doc命令0用例。 |
| [1ece消费信用](../../_tmp/20261115-iface-linux/kernel-1ece2f02-summary.json) | 600PASS、Clippy3处首败；受控backend／profile测试，不是PD最终consumer与真实supplier联验。 |
| [d999 SDK与补充guest](../../_tmp/20261115-iface-linux/kernel-d999f97b-summary.json) | Native/SDK152+后补guest21=173PASS，ClippyPASS但SDK5文件fmtFAIL。21项不倒算1104的完成数。 |
| [ee4c SDK](../../_tmp/20261115-iface-linux/kernel-ee4c6471-sdk-summary.json)／[正式Mint链](../../_tmp/20261115-iface-linux/kernel-ee4c6471-mint-chain-summary.json) | SDK63、fmt、ClippyPASS；Mint及guest-tool源码build PASS，首offline缺console保留。正式artifact chain是普通Rust integration测试1项，0PASS／1FAIL；在原518损坏注入遇PermissionDenied。原文件mode/nlink未采且现场已清理，不能反填原因或把前半到达当整链通过。 |
| [Mac已封结果](../../_tmp/20261115-iface-tests/pd-wip856-mac-final-summary-v1.json) | 最终25份普通Rust测试收据558PASS／55ignore：Context6、runtime86、HTTP29+27、Gateway410／55ignore。原Clippy、调度误运行及300s外部超时保留；same binary/source后继1800s完整PASS另记。 |

Mac [输入对应](../../_tmp/20261115-iface-tests/pd-wip856-to-d9aac-input-correspondence-v1.json)经[独立复算](../../_tmp/20261115-iface-sixth-preparation/input-correspondence-independent-check.json)：PD3050／Kernel1796共4846项（4840 blobs及6个删除）匹配Git固定版本。它仅对应原记录的scope输入，TS baseline/解析元数据仍由原receipt单独保留；不代表clean d9aac重跑或完整候选等价验收。

## PD共享包的四个责任边界

[有限源码边界审阅](../../_tmp/20261115-iface-sixth-preparation/product-boundary-trace-v2.draft.json)只读17个冻结边界文件、28个anchor，未宣称102个变化文件或原JS算法全部完成语义审计。

1. model-runtime共享异步driver、重试/计时/正文读取与重定向；产品端口供给header、audit、日期解析及dispatch。Gateway ambient facade是旧产品兼容壳，不是Kernel能力或受管fallback；SharedFactory22/resident未在558批重跑。
2. socket/proxy/TLS/SOCKS/解码独立成包；`for_scope`显式注入dial与执行器，ambient兼容构造仍存在。库存在或局部29测试通过不等于正式受管生产路径已接通。
3. trusted HTTP实现以原job、binding和准备事实控制checkout及scope收敛。`TrustedProjectLease`是marker trait，不能自行证明真实项目owner；公开snapshot真实性仍属可信组合根，真实RuntimeLease/provider/secret/生命周期会合未由这些局部27测试完成联验。
4. Context抽包具有模块、五操作端口与默认不支持错误；50条fixture case（1+5+41+3）对应6个Rust测试。真实history/artifact owner、summary/budget/managed adapter及原source-hash/Node重生成守卫未全部迁移。README中旧“未workspace/未测试”字样由当前Cargo成员和receipt纠正，未生产接线限制仍有效。

## 发布、完整性与剩余验收

原P7默认切换／迁移／退役、正式ProjectSession→generation owner、完整能力安装和consumer-demand真实产品闭环没有因本次入表关闭。冻结Kernel树内未见生产backend覆盖两个demand方法，release构造仅见测试；这说明本批还需要产品实际安装和资源owner联合验收，不能把后续批次的实现或测试回填到本批。

管理入表发布与Hub发行、push/main合入、产品部署分别记录。本批carrier仍0.1.8未发行候选，env0.1.11、types0.1.11、model0.1.8→0.1.9是独立身份轴；manifest/schema/API登记不代替真实安装。见[源码完整性报告](../../_tmp/20261115-iface-sixth-preparation/evidence/kernel-trace/integrity-check.json)与[API/身份核对](../../_tmp/20261115-iface-sixth-preparation/evidence/kernel-trace/source-review-checks.json)。后续P0/P1管理接受另批记载，不混为本批产品交付。
