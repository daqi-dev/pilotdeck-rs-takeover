# 完整模型输入检查：三表功能链示例

本例对应已实际写入并全量回读的三表：13条合同、598条任务及追踪、712条原运行，共1323行。任务表598条包含26条交付任务、463条上游处置、86条Kernel变更和23条产品变更。20项字段迁移及记录写入均有 `PASS_READBACK` 收据；这表示表结构、投影与关联按审定内容一致，不表示产品交付或功能联合验收通过。

本批事实来自第七v3冻结候选，账本SHA256为 `d91ecbe015355ad2bb12896df4c51b10cf01fdbbb20b1550f3a18c39f2da270b`；功能元数据SHA256为 `be9a94c6672073c7e03c2d5bc5e76885e6ec4c8403c792bcb08ab022f6625dc8`。后续Kernel第48轮Linux验证与第49轮首编不在这712条中，须按各自实际候选和原收据另批记录，不能回填旧运行。

## 记录类型与变更性质

任务表的实体类型固定为26条交付任务、463条上游增量处置、109条接入变更（86 Kernel＋23产品）。109条不再统称ifACE接口变更。原稳定ID保持不变。

性质分为ifACE公共接口、Kernel实现、PilotDeck能力适配、产品组装与入口兼容、测试与维护工具，可多选；598条各有分类依据、来源行canonical SHA、reviewer与时间。分类描述被审责任，不表示接收决定已经实施、测试通过或已合主线。没有按仓库自动生成接口类型；例如生产目录错误字段修正归Kernel实现，生产tool-core schema借用修正归PilotDeck适配，真实resident测试装置归测试与维护工具。

## 一个功能怎样贯通三表

功能稳定ID为 `iface.feature.model-input-inspection`，本例精确28条关联：原五条源码审阅、model合同及四条交付任务的需求责任关联、17条明确原运行，以及一条fd074远端master与dbef本地源码的明确差额审阅。相同合同/任务下的其他运行不会自动计入。

| 责任环节 | 显式变更 | 已知依据与限制 |
|---|---|---|
| 公开声明 | public-complete-input-inspection | ModelInputRequirements与默认Unsupported；声明不代替产品解析。 |
| 封组绑定 | sealed-input-ledger-inspection-binding | 共同账本绑定input/revision/job及metadata，缺失关闭。 |
| 实际内容门控 | request-profile-actual-media-gate | request profile按检查结果决定media能力，media_input=true同样检查。 |
| 派发前复核 | inspection-outside-lock-double-authorize | 锁外检查，前后授权/版本与claim复核；失败不派发。 |
| 产品能力适配 | product-media-inspection | 检查完整拼接请求中已声明content及嵌套块，未知/非法形状拒绝；opaque metadata/schema/tool raw不搜索，不读取reference、不授权路由。 |

需求责任只关联model合同、P2.model-context、P3.model、P4.loop、P6.contracts。这些关联不修改其合同层或交付状态。五份源码审阅按原ae40／2bb冻结身份记载；执行适用性从下表原actualVersions逐条原样取，不能以源码commit替代历史WIP执行。

## 原运行与首败链

下表每行都对应已有运行，不创建额外执行、不按组内test数制造记录。完整argv、版本与证据在独立元数据的功能关联JSON中。

| 原运行ID | 类型／原结果 | 本功能适用范围 |
|---|---|---|
| `iface.run.linux-0f081de1-first-host-model-session` | build／FAIL | 0f原Host首编六处Failed(String)模式E0532，Host测试未执行；是该功能候选的构建阻塞证据，不当行为FAIL或PASS。 |
| `iface.run.linux-0f081de1-first-model-session-ledger-all` | executed／PASS | 仅共同输入ledger该原命令/0f身份；整个库覆盖不全部归输入检查，证明范围限原关联封组/metadata约束。 |
| `iface.run.linux-0f081de1-first-iface-all` | executed／PASS | 仅ifACE原库测试执行，类型/默认契约边界；不能当真实Host/产品检查器执行。 |
| `iface.run.linux-ae40c323-first-host-model-session` | executed／PASS | ae40 Host model-session230 PASS中的input_profile9是明确相关子组（受控assessment）；scope12等不是本功能额外覆盖，230与9不重加。 |
| `iface.run.linux-ae40c323-first-host-resources` | executed／PASS | 原资源门/授权围栏受影响命令；相关检查后授权约束，非完整产品schema/media验证。 |
| `iface.run.mac-ae40c323-model-v1` | executed／PASS | Mac原model-session过滤命令及记录的source manifest/binary，输入检查相关子组；不把reported HEAD当clean源码或全量测试等价。 |
| `iface.run.mac-ae40c323-resources-v1` | executed／PASS | Mac原资源测试过滤范围，检查后的门控相关；不扩大为完整产品检查。 |
| `iface.run.mac-ae40c323-ledger-v1` | executed／PASS | Mac原ledger lib命令，封组与metadata相关；库内其他测试不自动属此功能。 |
| `iface.run.mac-wip-input-inspection-api-13-default-unsupported-run` | executed／PASS | legacy ModelSession未覆盖inspect_input，原独立负控立即Unsupported且其他方法未调用；原源码UNCONFIRMED dirtyWIP，只保原binary/命令身份，不回标ae40。 |
| `iface.run.pd-7d29-independent-build-v1-build` | build／FAIL | 原共同构建首次E0509（tool-core借用问题）阻塞；是该输入适配所在候选编译前置，不当媒体检查本身行为失败。 |
| `iface.run.pd-7d29-independent-build-v2-build` | build／PASS | 后继实际WIP共同构建成功，仅编译前置；源码manifest已变，原E0509保留，不当media行为测试。 |
| `iface.run.pd-7d29-media-v1-test` | executed／PASS | 原WIP media_inspection测试：声明content与嵌套块媒体分类/非法形状拒绝；opaque字段不猜媒体，不验证外部模型或路由授权。 |
| `iface.run.pd-91a1cbe3-factory-build-v2-build` | build／PASS | 原WIP factory仅no-run构建成功，不当16项执行PASS。 |
| `iface.run.pd-91a1cbe3-factory-v1-test` | executed／FAIL | 原factory16项中15PASS/1sealed input断言FAIL，完整保原WIP与失败；后续成功不覆盖。 |
| `iface.run.pd-91a1cbe3-factory-v2-test` | executed／PASS | 原factory修正后该WIP命令16PASS，封组检查与同一内容绑定范围；不回标2bb clean或正式resident产品组合。 |
| `iface.run.pd-1cceb75e-kernel0291-factory-v1-test` | executed／PASS | 原1c WIP+Kernel0291关联factory命令PASS，按source/binary身份适用；不推断后续候选重跑或tool/context入口通过。 |
| `iface.run.pd-e1b-kernel0291-resident-v6-resident` | executed／PASS | 真实e1b+Kernel0291原model-only preparation/real resident/loopback HTTP，1harness/3checks PASS（含request负控/unsupported binding）；test-only供应者，无tool/context或正式产品入口，不是3个Rust测试。 |

0f原Host首编六处E0532没有执行Host测试；后ae40230PASS的相关input_profile9只是子组，不能230＋9重复计算或把230全部说成输入检查覆盖。产品原factory15PASS／1sealed断言FAIL与后继16PASS保持各自WIP，不覆盖。原Unsupported负控含UNCONFIRMED源码身份，额外标记 observedOriginalIdentityOnly=true，只承认原binary与已观察命令事实。

e1b＋Kernel0291真实resident仅一次自定义harness／3checks：model-only原prepare、真实resident与loopback HTTP；不含tool/context或正式产品入口验收。它有明确关联范围，也不能据此把整个功能或P4/P6标完成。

## 分支、运行与等效功能分列

目标远端使用实际URL并单列remoteName：PD为配置origin的 https://github.com/daqi-dev/pilotdeck-rs-takeover.git ／main；Kernel为CodeHub agent-kernel.git／master。上游受审main源码身份与PD takeover目标库不因都叫main而混同。

本地提交只从原implementationCommit且本地Git对象可读时登记；不从sourceVersion推断实施提交。原actualVersions另存运行身份。对88条Kernel相关记录的33个不同本地实施提交，已分别实际运行 merge-base --is-ancestor 到2026-11-15T12:47:40Z观测的精确master fd074191，全部exit1。各条targetIntegration仅因此记NOT_MERGED，逐条candidate/target证据在kernel-master-precise-ancestry-v3.json；其他无本地实施提交的条目仍UNKNOWN。PD目标main尚无本轮精确head/祖先证明，保持UNKNOWN。所有remoteVisibility仍UNKNOWN，未枚举全部远端refs不填NOT_VISIBLE。commit非祖先仅说明该commit未并入，不说明目标源码没有等效功能。

Astra实际审阅fd074与dbef源码的独立结论已实读、核SHA及21份Git源码/行锚后追加为source_review：

- 远端正式model.request已通过内部payload codec校验messages、角色、工具历史和tools，再形成admission/queue payload；不能描述成“远端没有完整输入校验”。
- 远端session_open模式能力、Media门、声明附件降级、共同ledger/admission及worker/claim两次授权原已存在；不能把整个双授权链说成新加。
- 本地差额是完整request profile协商、sealed metadata prepare、锁外final inspector及input/revision/media要求门接入原授权派发链。远端非空content type和声明附件替换不等同每种媒体/data引用闭合。
- dbef Kernel树inspect_input仅公共默认实现与测试替身，无非测试覆盖；真实parser/resolver/同一不可变快照仍依赖可信产品adapter与组装另证。公开结果只有input_id/config_revision/requires_media_input，不是媒体/data引用闭合证明。
- 这条源码比较的真实对象是fd074与dbef，不把dbef纳入第七34的代码运行cutoff，不回填ae40历史测试或另加run。原712运行事实继续不动。

## 发布核验与交付边界

元数据全量schema检查及新列投影通过。712条原运行与第七v3来源账本逐条相等；相较发布前637条，原637条运行事实完整保留，新增75条原运行。独立功能、分类和分支列使行SHA按新schema重算；27条历史大事实仍采用既定完整事实引用，原事实未裁剪。

[字段迁移收据](../../_tmp/20261115-feature-schema-root/schema-receipt.json)实际 `PASS_READBACK`，20项字段操作已回读；[记录写入收据](../../_tmp/20261115-feature-schema-root/records-receipt.json)实际 `PASS_READBACK`，13项写入操作均 `VERIFIED_READBACK`。独立完整export对1323行46128个文本/事实单元及5152个LINK单元逐值核对一致，1238个既有记录ID保持，85个新增记录ID；六张旧表计数和内容SHA前后相同。以上管理核验不新增产品测试，也不把管理PASS计作功能PASS。

当前功能链只有上述明确证据适用范围。公开检查结果不是媒体/data引用闭合证明；Kernel源码无非测试inspect实现覆盖，产品可信adapter、不可变快照、正式组装、tool/context及完整入口仍需分别验收。

[发布后零差异计划](../../_tmp/20261115-feature-schema-root/zero-plan.json)确认三表创建、更新与操作均为0；[同计划dry-run](../../_tmp/20261115-feature-schema-root/zero-preview.json)实际 `DRY_RUN`、`writes=0`。远端完整export已绑定同一canonical SHA，LINK亦独立逐值核对一致。

发布后远端完整export的canonical SHA256为 `e9ddb7116e471d03f97c4c0ebbd98e8b9a35b938371a71c9c1c00ef78660be61`，与zero-plan及dry-run所校验的snapshot一致。dry-run不执行LINK写入；LINK零差异由[独立完整回读报告](../../_tmp/20261115-feature-schema-root/independent-publication-review-final.json)逐项解析远端record ID并匹配稳定ID证明。schema、records、snapshot及零差异原件的raw SHA256逐份保存在同一报告的 `publicationArtifactRawSha256`。
