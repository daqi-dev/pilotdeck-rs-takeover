# ifACE 接入三表维护

`state/iface-integration.json` 是本阶段事实账本。它与原后端 127 项叶子任务和网页联调账本分别维护；飞书只是投影。唯一执行交接仍为 `state/backend-handoff.md`。本工具不读取凭据，不管理旧六表，不运行产品、不调用模型、不自动建表或删除记录。root 是唯一外部写入人。

三表名称固定为 **ifACE 接入合同与能力映射、ifACE 接入交付任务、ifACE 接入验收记录**。合同分别记录接口定义、能力实现、正式入口接线、公开注入和当前组合验收五层事实。“能力实现”包括按责任分工提供的Kernel管理检查与具体能力包，不能理解为全部算法都写在Kernel内部。任务使用新 `iface.*` ID；关联 `backend.*` 只表示来源资产，不继承旧完成状态。验收表一条记录对应一个用例在一个明确版本组合下的一次执行或证据审阅，批次 ID 只作关联，不能当成额外一次运行。

历史初始账本为13合同、26交付任务、0验证记录。此前 `sync-v2` 快照为13合同、26交付、463上游处置、38项ifACE变更、145条验证记录。[第三批历史回读收据](../../_tmp/20261115-iface-latest-preparation/sync-v2.json)为 `PASS_READBACK`：当时三张物理表分别有 **13、544、238 条记录，共795条**；任务表的544条包括26交付任务、463上游处置、50项Kernel变更和5项产品变更，该批身份保留。

[第四批历史回读收据](../../_tmp/20261115-iface-fourth-preparation/sync-final.json)为 `PASS_READBACK`：三张物理表分别有 **13、571、353 条记录，共937条**。任务表包括26交付任务、463上游处置、69项Kernel变更和13项产品变更；比第三批新增27项变更追踪、115条验证记录。第四批当时的账本SHA256为 `ed1d2648d053dc20d6014a9ad283a4d00b6cab4dc1ca05ba8e47bfd4ed1d81b6`；[正式投影](../../_tmp/20261115-iface-fourth-preparation/projection-final.json)仅来源路径与已审v4不同，表内容和事实完全相同。原六表的记录数及内容哈希在此次同步前后完全相同，交付分母仍为26，旧后端127项独立统计。管理同步回读只证明数据和关系匹配，不表示产品通过。

条款引用与任务来自批准计划，来源读取或记录增加不自动改变合同审计层或交付状态。早期术语审查覆盖94个稳定ID，此前 `sync-v2` 对685条物理记录做了显示用语复核。第三批对795条物理记录完成显示语义审阅，并对两项修订独立复核；238条验证事实JSON和全部795项首次失败在该次修订前后保持不变。第四批[独立显示语义审查](../../_tmp/20261115-iface-fourth-terminology-independent/display-semantic-review-final-v4.json)审查142个新增行及29个既有变更行，766个逐字未变行继承第三批审查，关闭44行共59个显示单元修订。第四批v2到最终v4的937条事实JSON及首次失败逐字不变；原第三批238条运行整行（含事实JSON及首次失败）保持不变。显示审阅及证据身份核验不表示全部937条都重新做过源码实现审查或联合验收。

第四批表中证据截止为 **PilotDeck `e1b759b0`／Kernel `f7931007`**；真实resident v6实际组合仍为 **e1b＋Kernel `0291dbe0`，1次自定义harness／3项检查PASS**，没有e1b＋f793组合PASS。后续Kernel `190ad`大矩阵及新driver开发中代码均不在本批。新增Kernel解读分别见[8a2e公开pull与冻结首败](../../docs/iface-integration/kernel-http-pull-8a2e.md)、[276／aba／52后续修复](../../docs/iface-integration/kernel-http-followup-52a7.md)、[f793显式fixture与实际范围](../../docs/iface-integration/kernel-explicit-fixtures-f793.md)，产品变更见[独立来源记录](../../state/iface-product-followup-e1b.json)；历史0291 guide保留原身份。

最新[第五批最终回读收据](../../_tmp/20261115-iface-fifth-preparation/sync-final-links.json)为 `PASS_READBACK`：**13条合同、576条任务及变更记录、443条验收记录，共1032条**。576条物理任务记录分为26条交付任务、463条上游处置、74条Kernel变更、13条产品变更；本批新增5条Kernel变更和90条验收记录，没有关闭交付任务。完整账本SHA256为 `7b9009046fba96a5f5f7480b611a9ee1dd85cf5977fe21fc53fd0cbe67ce6c08`。原353条验收记录、六张旧表及既有首次失败保持原状；26交付分母和旧后端127分母各自独立。

第五批[独立显示语义复审](../../_tmp/20261115-iface-fifth-terminology-independent/display-semantic-review-final-v4.json)已关闭5类问题、7条相关记录，明确API消费者检查、零用例文档命令、受影响范围与完整矩阵、Mac实际dirty输入及配置/产物身份的区别。显示审查、源码审查、构建成功、实际运行和管理同步分别记载。表内证据截止到 **Kernel `5d6d4569`及当时Mac共享driver/factory的108项dirty输入验证**；后续`856`至`ee4c`和当前产品候选的结果另批追加，不拼成同一组合通过。第五批代码解读见[HTTP准备来源与实际清理](../../docs/iface-integration/kernel-preparation-cleanup-5d6.md)。

本批同步保留了两次首败：第一次请求超过已观测的5MiB请求限制；按字节分批后，27个新run的大事实字段在CLI直接回读中为空。具体丢失位置未确定，不能据此宣称官方单格阈值。经[独立审查](../../_tmp/20261115-iface-projection-review/final-frozen-review-v3.json)，只更新这27条的“事实JSON”及随之重算的“行SHA256”；事实列引用完整Git事实的稳定ID、canonical SHA256和字节数，其他文本列保持原值。完整事实未裁剪，旧353条投影未改。定向修复及最终关联写入分别全量回读；原失败收据、journal备份和root逐项处置依据保留，未重建已存在记录。表中任何PASS仍只适用于对应运行范围。

## 离线检查和投影

从仓库根执行，批次产物放在新 `_tmp/YYYYMMDD-.../` 目录：

```sh
python3 scripts/iface-integration-status/extension.py check --ledger state/iface-integration.json
python3 scripts/iface-integration-status/extension.py schema --out <批次目录>/schema.json
python3 scripts/iface-integration-status/extension.py project --ledger state/iface-integration.json --fact-references state/iface-projection-fact-references.json --out <批次目录>/projection.json
python3 scripts/iface-integration-status/extension.py plan --projection <批次目录>/projection.json --remote <批次目录>/remote.json --out <批次目录>/plan.json
python3 scripts/iface-integration-status/test_status.py
python3 scripts/iface-integration-status/test_payload.py
python3 scripts/iface-integration-status/test_fact_cells.py
```

当前投影必须显式携带 `--fact-references state/iface-projection-fact-references.json`。该policy固定27条引用的完整事实哈希，并保护旧353条事实；每次投影重读完整账本核验，不删除或缩短原事实。未来追加运行不会改写已有引用。单格50,000 UTF8字节、每请求4MiB及200行是本地写入预算；单项超预算停止，不截断内容。旧已验证大格只读保留，不能以预算修复为理由重写。

`repair_fact_cells.py`是本批缺格事件的受限修复工具，不是普通同步的自动恢复开关。它绑定原投影、未变账本、完整before快照、27个recordId及每行哈希，只更新事实引用和行哈希，并持有同一写锁与独立repair journal。其成功不会自动清除原write journal；root必须在确证原创建身份及修复后内容后另行追加明确resolution。原未知写入守卫、失败收据及操作备份始终保留。

编辑前保存旧账本，随后使用 `check --previous <旧账本>` 核查记录没有删除、验收原件没有改写。`--verify-evidence-root <仓库根>` 会实际读取并核对全部证据文件 SHA256；普通检查只核字段与引用，不声称已读取证据。验收原件可以位于隔离批次目录；不得把凭据或用户正文入账。

任务状态 `null` 表示未核实，其余状态为 `planned / in_progress / needs_verification / needs_integration / delivered / blocked`。已交付需要明确 reviewer、reviewedAt、scope、appliesToVersions 和 accepted run IDs；每个验收用例都有 PASS，版本适用范围一致，前置交付关闭。一个局部 PASS 不关闭整项任务。`verificationKind=runtime` 的任务必须有实际执行记录；离线管理测试或证据审阅不能替代它。

运行记录中的 `actualVersions` 永远保存原受测身份；跨版本继承使用 `executionKind=evidence_review` 和单独的 `inheritance.appliesToVersions / scope / reason / sourceEvidence`。不得把继承说成重跑。`NOT_RUN` 不登记实际受测版本、完成时间或通过。原失败记录不可变，后续修复另加记录，首次失败由原记录生成。

构建记录使用 `executionKind=build`，管理工具与飞书回读使用 `management`，均不能关闭 `runtime` 任务。历史源码或二进制身份缺失时，以 `identityStatus=UNCONFIRMED` 和明确原因保留；禁止拿日志哈希冒受测产物。当前文件已被后续构建覆盖时，其事后 SHA 仅放 `observedArtifacts`，不回填为历史实际版本。未保存的开始/结束时间保持空，`recordedAt` 仅表示补录时间；未封存的 argv/config 也显式标未确认。包含未知身份的证据不能关闭交付。

## 独立绑定与真实关联

历史初次建表时，由root核实CLI建表API，创建三张表及TEXT字段，取得实际table IDs后创建原六个单向LINK字段。当前沿用已核实绑定并包含批准的扩展字段与“关联交付任务”关系；不要重新建表或用初始字段集合覆盖当前绑定。`schema` 输出逻辑字段及目标表类别；不猜数值类型。关系如下：

| 来源表 | LINK 列 | 目标表／账本字段 |
|---|---|---|
| 合同 | 接入任务 | 任务／taskIds |
| 任务 | 接入合同 | 合同／contractIds |
| 任务 | 前置接入任务 | 任务／dependsOn |
| 任务 | 验收执行 | 验收／runIds |
| 任务中的变更／上游记录 | 关联交付任务 | 原26条交付任务／taskIds |
| 验收 | 接入合同 | 合同／contractIds |
| 验收 | 接入任务 | 任务／taskIds |

辅助文本列中的稳定 ID 不冒充飞书关系。同步先创建文本记录，完整回读取得真实 record IDs，再计算并写 LINK，最后按 ID 集合回读。不会按名称或最新版本猜关系。

绑定放在 Git 元数据目录下，例如 `git rev-parse --git-path iface-integration/binding.json` 所指位置，不入库。绑定必须显式给出：

```json
{
  "schema": "iface-table-binding/v1",
  "projectId": "pilotdeck-iface",
  "baseToken": "实际主Base token",
  "schemaConfirmedByRoot": true,
  "protectedTableIds": ["旧表1", "旧表2", "旧表3", "旧表4", "旧表5", "旧表6"],
  "tables": {
    "contracts": {
      "name": "ifACE 接入合同与能力映射",
      "tableId": "新合同表ID",
      "fields": ["schema中该表全部TEXT与LINK列名"],
      "linkFields": {
        "接入任务": {
          "link_table": "新任务表ID",
          "bidirectional": false,
          "targetKind": "tasks",
          "sourceField": "taskIds"
        }
      }
    },
    "tasks": {"说明": "填写对应实际表身份、完整fields和当前四个linkFields"},
    "runs": {"说明": "填写对应实际表身份、完整fields和两个linkFields"}
  }
}
```

示例不是可执行绑定。示例仅展示历史初始字段，当前绑定必须使用扩展后实际schema。所有六个旧表ID必须不同且与三个受管表分离；LINK目标也只能是本组三个受管表。首次绑定前要核实真实表名、字段类型、LINK 目标和单向属性。运行时重新检查实际字段集合和 TEXT/LINK 类型。

## root 同步流程

`root_apply.py` 通过 `--engine` 显式复用已有 `scripts/project-status/project_status.py` 的 `Lark` 类。当前产品冻结分支未携带旧管理脚本，因此第一次可指向 `/Users/da/ws/pilotdeck-rs-cc2c/scripts/project-status/project_status.py`；其他机器须提供这份已核实管理引擎。引擎封装 `lark-cli base` 的完整分页、错误响应、被忽略字段和大请求处理，不通过复制旧账本带入历史状态。Python 仅用标准库。

```sh
python3 scripts/iface-integration-status/root_apply.py export --engine <已核实project_status.py> --base-token <主Base> --tables <独立binding.json> --out <批次目录>/remote.json
python3 scripts/iface-integration-status/extension.py plan --projection <批次目录>/projection.json --remote <批次目录>/remote.json --out <批次目录>/plan.json
python3 scripts/iface-integration-status/root_apply.py apply --engine <已核实project_status.py> --base-token <主Base> --tables <独立binding.json> --projection <批次目录>/projection.json --plan <批次目录>/plan.json --out <批次目录>/preview.json
python3 scripts/iface-integration-status/root_apply.py apply --engine <已核实project_status.py> --base-token <主Base> --tables <独立binding.json> --projection <批次目录>/projection.json --plan <批次目录>/plan.json --out <批次目录>/sync.json --execute
```

默认 apply 只是预览，写入数为零；只有 root 加 `--execute` 才写。执行前重建投影、重读远端并核对计划，执行期间每次写前检查来源漂移。未知行、重复 ID、缺身份、未知字段、非完整快照、验收原件改写都会停止。写操作仅允许受管三表的批量创建和更新，没有删除、自动 schema 修改或旧表写入路径。

绑定旁的 `*.write-journal.json` 持久记录每次不确定写入；换一个收据文件不能绕过它。请求发出前先落 journal；超时或未知回执后停止，必须先回读。只有全部对应记录及值已实际可见，才允许新计划继续；仍不可见的创建不能盲重试，需 root 结合服务端历史明确处置。不要删除 journal 来消除错误。执行期间使用独占文件锁，防止两次同绑定同步并发。

每次快照都保存旧六表记录数与内容哈希，写后必须保持一致。外部并发修改也会导致计划漂移，需要重新审阅，不自动覆盖。最终文本和 LINK 均无差额才记 `PASS_READBACK`；无变化记录 `NO_CHANGE`，不冒充新同步或新产品验收。首次失败收据保留。

历史首轮已有四个filter/group视图的回读记录；它们只证明当时配置。当前更新是否包括某个视图或页面布局须按对应回读收据说明，不能由数据同步推导未核实的仪表盘或布局也已完成。

### 上游处置与 ifACE 变更追踪

任务表的「记录类型」分为「交付任务」「上游增量处置」「ifACE变更」。后两类来自独立数组，使用稳定 ID，在同一物理表呈现；它们不是交付任务，不能作为前置任务，也不加入原 26 项交付分母。汇总分别计数。「关联交付任务」是单向关系，仅可指向原交付任务；前置关系仍仅接受原交付任务。记录保留上游精确版本和路径、审阅层级、处置理由、实现与实测状态、合入状态、可核实的提交和实际验收记录。路径清单、路径及依赖清单与生产语义审阅是不同深度，不能把清单处置理解为语义审计通过。上游接收决定、代码实施、本地候选吸收、实际测试、远端推送与main合入分别记录；空提交或未核实不能理解为已交付。

投影中附加来源文件也按 SHA256 校验，每次写入前重新生成投影。变更绑定时使用 `root_apply.migrate_journal`，仅允许旧绑定与 journal 精确匹配、待核实操作为空、表身份和既有关联不变，字段仅允许已批准的「Kernel实现」更名为「能力实现」及既定新增字段。保留原 journal 副本与全部 attempts，并记录哈希迁移；禁止通过新绑定绕过不确定写入。
