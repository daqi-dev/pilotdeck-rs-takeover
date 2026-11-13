---
id: A7-STDIO-REBIND-EXACT-001
---

## 类别

不可避免的身份表示差异与有源码依据的并发标题完成次序；仅接受已固定三份原件的有限表示，不改变产品或一般比较规则。原始严格 self DIFF 8 / pair DIFF 69 继续并列，旧 self 8 / pair 157 的历史结论不变。

## 基线依据

固定 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。原 CLI 场景 `agent-loop-sidecar-stdio`、场景 SHA256 `60f19ad90cf9dd0756c6d4abaaffcd9fc7758ec624983fa478b99d06daf40544`、harness `eb575d930fed5a0b6959eafe07b925df6cfc170c` 的实际 capture 为 `5f9f752b-e5a8-48ef-b1c3-5b5d74c5e8de`。三原件均 23 条持久记录，包含唯一 started / accepted / terminal journal。TS 两份 fresh provider title 在原 index 3，started 在 4；Rust 恰相反，accepted 在 5、terminal 在 19。

TS `resources/PilotDeck/src/agent/modules/transport/agentLoopSidecarServer.ts:96,101–102` 使用 Math.random base36 构造非空 module instance / connection generation，stream 同属不透明字符串。Rust 身份字符串内含 UUID，冻结比较器的 UUID 首次出现别名会受它们影响。TS `resources/PilotDeck/src/agent/turn/TurnRunner.ts:262,290,578–588` 启动标题 promise 后继续 loop，provider 标题完成时异步保存；没有标题完成先于 journal started 的同步边。因此只解释本批固定邻槽次序，不推导一般顺序豁免。

原件目录为 `_tmp/20261113-e0-resume/e99ee172-agent-loop-sidecar-stdio-first/run/raw/`，文件依次为 `agent-loop-sidecar-stdio.ts-1.json`、`.ts-2.json`、`.rust.json`，SHA256 分别为：

- `a35485cb027083ad22843a511655f8b46daa733f6572d07606d37c95c721ef90`
- `abcaf5c56fb8a8c260568faa7cccb0f859f35efc2ba6dad355975ca6d73dbaa9`
- `4438cfe1ccca372721ac71d2b76b44d170c10476afd2e9cf0d437b95d5b8f191`

## Rust 行为

capture checkout 是 `c88650f6d89bd839d900e6256aa65bdca82e0b3d`，实际运行二进制来自 build45 `e99ee172e25c29c34ce299f03808773f16f2ec59`，不能将采集 checkout 冒作新 binary。CLI SHA256 `cd42c130d4653dd1a7f47bd90e83e135ce8fa801cf24516a74683c54854d01f0`，sidecar SHA256 `dc014b0e8b5a576d225b7d0649633c0267d4484d63b9fbf66feba9224190b455`，runtime 不变证明 SHA256 `d62e7c2c3a1002015d000c4bf60e795b49f35ccbf7648cd0cc7890325f05896f`。

Rust `src/pilotdeck-gateway/src/runtime/turn/runner_tail.rs:192,264,570–574` 独立启动标题任务，标题与 started 的本批相邻次序允许不同。三 journal 的完整 run / operation / request / session / turn / binding 关联、同 stream、terminal completed / lastAppliedSequence 10、实际 result / messages / seed、完整 parent chain 与 checkpoint 均通过固定原件检查。旧采样缺三条 journal 是历史产品缺口，本次真实原 CLI 采样已捕获三条；不将旧缺口解释为随机身份。

## 范围

只适用注册文件 `scripts/bbx/registrations/stdio-rebind-exact-20261114.json` 中完整固定的 capture、三 raw SHA、source/build/bin、scene/harness、全部 evidence inputs、原比较器闭包及关联收据。

唯一许可是：每份原件 started / accepted / terminal 三记录的 `binding.moduleInstanceId`、`binding.connectionGeneration` 六处，以及 accepted / terminal 的 `streamId` 两处，以三个彼此不同的类型别名表示。先核同类型重复值全等、完整 journal tuple 和 stream 关联，再替换八处。另只交换 Rust 原 index 3 / 4 的同 turn fresh provider title 与 started payload，所有原槽 `sequence / entryId / parentEntryId / createdAt` 保留，projection 完全不改且不得引用被交换 entry。变换必须可逆恢复整个原 JSON。

不扩展全局 FLOAT_OVER，不移动 accepted / terminal，不删除字段，不豁免其他业务/provider/标题内容。未来原件、其他 binary、一般 opaque 匹配、一般标题次序、resume 或重启不自动适用。本次短采样未捕获实际 sidecar child PID / lifetime，也没有独立 sidecar wire sequence trace；durable lastAppliedSequence 10 与其他拓扑 PID 证据不补造这些观测。

## 机器判据

独立入口 `scripts/bbx/accepted-stdio-rebind-exact.mjs` 固定 registration SHA，仅接受一个显式全新绝对输出目录，拒绝覆盖。它先核全部不可变原件、原 `validateArtifact`、23 行唯一 entry / sequence / parent chain、三个唯一 journal 的完整 tuple、typed atoms distinct、stream、terminal/result/messages/seed、same-turn 标题及最终 snapshot、projection anchor/copies 与原始 WS completion。随后只做上述有限变换，调用 SHA256 `f64726bf123f00fa7e8b6ae2739de9be7dc85be92ad4e84ac2c470d21c166a0f` 的原完整比较器，原 diff / scenario / raw 均不改。

在存在固定原件和 Node 22.22.0 同 SHA 离线解释器的工作区运行（将输出目录换成此前不存在的绝对路径）：

```sh
/opt/homebrew/Cellar/node@22/22.22.0_1/bin/node scripts/bbx/accepted-stdio-rebind-exact.mjs --output /absolute/new-owned-output-directory
```

要求原 self `DIFF 8`、原 pair `DIFF 69` 与有限派生 self / pair `SAME` 同时成立。24 个 SHA / identity / journal / tuple / stream / typed-atom / parent / projection / sequence / outcome / seed / result / messages / title / provider-count 负控必须全部拒绝；另 provider 业务请求变更应 `DIFF 1`，标题三副本一致变更应 `DIFF 3`。负控仅对固定原件的内存克隆绕过外层 raw SHA，以验证内层守卫；无任意输入接纳接口。

私有作者结果 `_tmp/20261114-final-stdio-residual-review/rebind/exact-candidate/output/ready.json` SHA256 `53b07fabac8e264328a17cdb81e7b2f6177d3f7a944432c456b5fbb13c19a588`；Terra 非作者独审 `_tmp/20261114-stdio-rebind-terra-review/review.md`，含独立视图逐对象核对、六关系负控与原比较器业务内容控制。私有实现调试首败及输出保留，不能由最终通过抹除。公共迁移须再核实际完整结果及非作者 delta 审；该独立入口不替换原严格判分。

## 批准记录

2026-11-14 已批准。主协调依据本会话用户项目自主决策授权正式接受，未冒称用户逐字批准具体规则。持久决定为 `state/decisions-20261114.md:105`，标题“stdio 原入口：固定三原件的身份表示和标题邻槽次序”。批准的私有候选 SHA256 `079cd9068ff19df4d5ee9813dfd9ceadd62c11edf0426458ce7d1357d74ea753`、manifest `45ad740416fb2333545d734aeb950322c5f8e791b2727a56d0db3c2617805039`。

授权只新增独立公共固定 registration / 入口及本条目，机械迁移限 ROOT/HERE、manifest 路径及 SHA、批准元数据与全新输出目录显式参数。冻结、原严格比较器、场景、raw、产品和既有差异条目不改；不把这一精确接受转记为未来采样通过或整模块交付。
