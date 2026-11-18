# t020 旧数据备份迁移与验证——设计稿

> 任务 `cli.task.t020`（S4 转换迁移与发行，责任层：领域服务）。2026-11-18 定稿
> （S4 兼容层进程交付；待 S3 集成者验收登记）。
> 本文是设计稿，不是台账、不是合同正本。所有操作语义以
> `state/cli-v1-operations.json`（285 操作合同，approvalRevision 见该文件头部）为正本；
> 旧数据事实以基线源码 `resources/PilotDeck/`（固定 `cc7c4e6`）为依据，逐条带 文件:行。
> 引用约定：`operations.json:<op>` 指该合同中对应 id 的条目；`007 §x` 指
> [007](007_agentcpm-pd-cli-architecture.md) v0.7.1（与 cc2c 同步副本）。
>
> 红线（AGENTS.md）：本设计与后续实现一律**不默认读真实
> `~/.pilotdeck`**；源根必须由调用方显式给出。测试用隔离临时目录构造旧数据形态。

## 0. 任务定位

- 交付物（台账 t020 fields）：「停止源写入、备份、暂存转换、验证提交及恢复工具；旧ID映射」。
- 实现策略（同上）：「配置运行管理由 cli.task.config 承担；本项仅负责持久数据迁移闭环」。
- 进入条件：`cli.task.wp0-gate`（已收口）、`cli.task.t012`（项目/会话/历史/归档域）、
  `cli.task.t018`（记忆与快照域）、`cli.task.config`（配置域）——均未完成，故本轮只交付设计。
- 完成条件（验收口径）：「交付物可由公开产品操作验证；本阶段主验收与适用故障/权限/并发
  用例通过并保存版本身份。较晚阶段关联用例是后续集成门。」
- 关联合同：`cli.contract.g010`「数据安全」——「inspect、backup、migrate、verify 均显式；
  迁移前验证备份和中断恢复」；验收要求「检查不产生迁移写入；迁移前验证备份可读；中断后
  源数据与已提交目标状态可核对，未验证完成不得报告迁移成功」。
- 关联能力：`cli.capability.a038`（数据检查与备份，4 个 data.* 操作）、
  `cli.capability.a039`（数据迁移与验证，5 个 migration.* 操作）。a039 关键语义：
  「源实例停止写入后检查并备份；在隔离目标暂存转换，验证后提交。失败保留原数据与明确
  恢复步骤；连接不自动迁移，不允许新旧双写同一数据根。」

## 1. 源与目标：两个数据根，一个权威

| 侧 | 解析 | 依据 |
|---|---|---|
| 源（旧） | `PILOT_HOME` 环境变量，默认 `~/.pilotdeck` | `src/pilot/paths.ts:9,20-22` |
| 目标（新） | `AGENTCPM_PD_HOME`，默认 `~/.agentcpm-pd`，权限 0700，与 `~/.pilotdeck` 分开 | 007 §8；实现见 `agentcpm-pd-client/src/config.rs:37-47` |
| 目标权威 | `<home>/v1/state.sqlite`（rusqlite 0.39.0 bundled，WAL） | 007 §7.3；`public_v1/store.rs:242-253` |

- 合同约束（operations.json:migration.plan acceptance）：「源/目标根不能相同或嵌套到互相
  覆盖」；`INVALID_DESTINATION` / `FORMAT_UNSUPPORTED` / `WRITER_ACTIVE` 为 plan 的合同错误。
- 新侧 v1 现有表：`meta / projects / sessions / runs / run_queue / interactions / events /
  idempotency_records`（`public_v1/store.rs:2111-2197`）。**目前没有旧 ID 映射表**——本设计
  §5 提出新增（DDL 属实现层决策，需 S3 集成者登记）。
- v1 现已交付域仅 system/project/session/run/queue/interaction（`public_v1/domains/mod.rs:79-84`），
  `data.*` / `migration.*` 九个操作均未注册（返回 `CAPABILITY_UNAVAILABLE`），符合 S3/S4 排期。
  九个操作在 285 合同与命令树中均有条目（`state/cli-v1-command-tree.json` commands
  `data inspect|create-backup|list-backup-entries|read-backup-artifact`、
  `migration plan|stage|verify|commit|recover`，参数绑定见 §4 各步）。
- 复用线索（非权威）：`pilotdeck-session-migration` crate 已有无回退导入、静止审计
  （`audit_jsonl_transcript`）、幂等导入 key（`importer.rs:61,85,107`）。t020 实现可参考其
  静止性判定思路，但落点必须走本设计的公开操作序列，不得直接复用其 ContextLedger 通道。

## 2. 旧数据形态事实清单（基线 cc7c4e6，只读源码推断）

`<PILOT_HOME>`（默认 `~/.pilotdeck`）下的持久数据：

| # | 数据类 | 位置与形态 | 依据（文件:行） |
|---|---|---|---|
| A1 | 全局配置 | `pilotdeck.yaml`（YAML；`adapters.feishu{enabled,appId,appSecret,connectionMode,domainName}`、`adapters.wecom{enabled,token,extra}` 等） | `src/pilot/paths.ts:10`；`src/cli/commands/gatewaySetup.ts:8,258-264,472-478,626-631`；`src/pilot/config/loadPilotConfig.ts:69-76,358` |
| A2 | 渠道凭据（微信） | `weixin-credentials.json`（`{baseUrl,botToken,accountId}`，明文 JSON） | `src/cli/commands/gatewaySetup.ts:9,597-603` |
| A3 | 项目登记 | `projects/<projectId>/`，目录内 `.cwd` marker 文件存规范路径，为权威；无 marker 时回退 legacy slug | `src/pilot/paths.ts:36-39,80-87,118-151` |
| A4 | 项目 ID | legacy：路径 slugify（lossy，非 ASCII 可碰撞）；collision-resistant：`<slug>--<sha1(path)[:10]>` | `src/pilot/paths.ts:65-75,110-116` |
| A5 | 会话转写流 | `projects/<projectId>/chats/<safeSessionId>.jsonl`，每行一个 `AgentTranscriptEntry`；追加写 0600、目录 0700；崩溃残留半行时补换行保留残骸 | `src/session/persistence/JsonlSessionPersistence.ts:20-65`；文件名 `src/session/storage/ProjectSessionStorage.ts:405-408,239-244` |
| A6 | 转写条目 | `type` ∈ {accepted_input, assistant_message, tool_result_message, durable_message, agent_status_message, file_artifacts, …, turn_result, control_boundary, session_metadata, subagent_*, plan_todo_*, goal_changed}（41 种）；公共字段 `type/sessionId/turnId/sequence/createdAt/entryId?/parentEntryId?` | `src/session/transcript/TranscriptEntry.ts:19-69` |
| A7 | 会话元数据 | 转写流内 `session_metadata` 条目（title/aiTitle/tag/linkedPullRequest/mode…），含 `isSnapshot` 尾部重申 | `src/session/metadata/SessionMetadataStore.ts:22-59,80-94` |
| A8 | 旧 session ID | 字符串 `sessionKey`：`${channelKey}:project=${projectKey}:s_${uuid}`（新会话）；`tui:project=<cwd>:default`（TUI）；`web:s_<uuid>`（Web fork，win32 分隔符为 `-`）；`cron:${taskId}`；`api-${uuid}`；always-on 辅助 `always-on-(discovery|workspace|report)[-_]…` | `src/gateway/client/InProcessGateway.ts:2030-2034`；`src/adapters/channel/tui/TuiChannel.ts:85`；`src/web/server/forkSession.ts:45-48`；`src/cron/runtime/CronRuntime.ts:599-601`；`src/adapters/channel/api-server/ApiServerChannel.ts:239`；`src/session/search/NodeSessionSearchPort.ts:16,257` |
| A9 | 文件名清洗 | `sanitizeSessionIdForPath`：非法字符→`-`，防 `/` 逃逸成目录树 | `src/session/storage/ProjectSessionStorage.ts:225-244` |
| A10 | run 登记 | `runs/registry.json`（快照式原子替换，schemaVersion 1；record：`{projectKey?,sessionKey,runId,ownerPid?,state,revision,lastSeq,requestMaterial,acceptedAt,updatedAt,result?}` + per-run events 数组；伴生 `registry.json.lock` 文件锁，陈锁 60s） | 挂载处 `src/cli/createLocalGateway.ts:996`；实现 `src/gateway/run/RunRegistry.ts:8-22,41,54-67` |
| A11 | 旧 run ID | `input.runId ?? randomUUID()`（客户端可自带） | `src/gateway/client/InProcessGateway.ts:903-911` |
| A12 | 工具产物 | `<projectRoot>/.pilotdeck/tool-results/<safeSessionId>/`（项目内，gitignore） | `src/session/storage/ProjectSessionStorage.ts:415-419` |
| A13 | 文件历史 | `projects/<projectId>/chats/<safeSessionId>/file-history/`；备份/恢复在 `session/filesystem/` | `src/session/storage/ProjectSessionStorage.ts:420`；`src/session/filesystem/{createBackup,restoreBackup,backupNaming}.ts` |
| A14 | 子代理转写 | `chats/<safeParentId>/subagents/<safeChildId>.jsonl` | `src/session/storage/ProjectSessionStorage.ts:423-428` |
| A15 | cron 任务 | `cron/projects/<projectId>/tasks.json`；`CronTask{schemaVersion:1,taskId,message,schedule,status,sessionKey,channelKey,projectKey?,mode?,timezone?,createdAt,updatedAt,nextRunAt?,lastRunId?,revision?…}`；`taskId=randomUUID()` | `src/cron/storage/CronPaths.ts:17-33`；`src/cron/protocol/types.ts:26-45`；`src/cron/runtime/CronRuntime.ts:225` |
| A16 | cron 运行 | `cron/projects/<projectId>/runs/<runId>.events.jsonl` 与 `run-history.jsonl`；目录级迁移锁（10min 陈锁） | `src/cron/storage/CronPaths.ts:31-38`；`src/cron/storage/CronStoreMigration.ts:26,29-39` |
| A17 | skills | 用户级 `~/.pilotdeck/skills/<slug>/SKILL.md`；项目级 `<projectRoot>/.pilotdeck/skills/`；内置只读随发行 | `src/extension/skills/types.ts:9-22`；`src/extension/skills/SkillManager.ts:73-74,238-244` |
| A18 | plugins | `~/.pilotdeck/plugins/`（全局）与 `<projectRoot>/.pilotdeck/plugins/` | `src/pilot/paths.ts:56-63` |
| A19 | memory | `<PILOT_HOME>/memory/`（memory root） | `src/pilot/paths.ts:32-34`；`src/pilot/config/loadPilotConfig.ts:133` |
| A20 | always-on | `<PILOT_HOME>/always-on/`（state/plans/cycles/reports/runs/events/locks/worktrees/snapshots 子结构） | `src/always-on/storage/AlwaysOnPaths.ts:4-36` |
| A21 | 其他运行期目录 | `gateway/`（如 `sdk-task-budget-ledger.jsonl`）、`browser_screenshots/`、`runtime/` | `src/cli/ProjectRuntimeRegistry.ts:1513`；`src/cli/createLocalGateway.ts:561-564` |
| A22 | 旧 gateway 档案方法 | `native_archive_manifest/entries/artifact`、`export_session_transcript`、`restore_session_transcript`（a038/a039 操作的 sourceIds 承接来源） | `src/gateway/protocol/frames.ts:39-41,52-53`；实现 `src/cli/createLocalGateway.ts:1183-1231` |

要点：
- **没有数据库**。旧持久层全部是 JSON/YAML/JSONL 文件；会话与 run 的关系、事件游标都靠
  JSONL 内的 `sequence`/`seq` 字段表达（`RunRegistry.ts:22`、`TranscriptEntry.ts:62-69`）。
- 旧实例**没有单一进程状态文件**；活跃写者判定可用信号：`runs/registry.json.lock`
  可否独占获取（可获取即写者已停/死，`RunRegistry.ts:62-67`）、cron 目录迁移锁
  （`CronStoreMigration.ts:26`）。全量停写声明（`data.inspect` 的 `activeWriter` 字段）
  依赖这些锁的联合检测——列为 §13 待核实项 T3。

## 3. 总序列（六步闭环）

```
(0) 停止源写入（人工/运维动作，工具只验证不承诺）
      │  验证: data.inspect → activeWriter=null、issues 无 error
      ▼
(1) 备份  data.create_backup(dataRoot,destination,includeSecrets=false)
      │  抽样: data.list_backup_entries / data.read_backup_artifact（备份可读性，g010）
      ▼
(2) 计划  migration.plan(sourceRoot,targetRoot,targetFormatVersion)   [preview 零写]
      ▼
(3) 暂存  migration.stage(planId,planHash,backupArtifactId,sourceRevision)
      │        → stagingId + idMapArtifactId + convertedEntries + diagnostics
      ▼
(4) 验证  migration.verify(stagingId,planHash)
      │        → verified + verificationId + reportArtifactId + failures[]
      ├── verified=false ──► migration.recover(stagingId,action=discardUnpublished) ─┐
      ▼                                                                                │
(5) 提交  migration.commit(stagingId,verificationId,planHash,expectedTargetRevision)   │
      │        → published=true + targetRevision + recoveryArtifactId                 │
      ▼                                                                                │
(6) 恢复  migration.recover(stagingId,action=restorePreviousTarget|discardUnpublished)◄┘
          （提交后发现问题时；受 REVISION_CONFLICT 保护）
```

编排不新增任何合同外字段或操作：九个操作在命令树已有 CLI 形状，调用方（用户脚本或文档）
按序串联；本设计不为它发明新 recipe（命令树目前唯一 recipe 是 `server serve`，
`cli-v1-command-tree.json` recipes）。

## 4. 逐步语义（输入 / 结果 / 失败 / 中断重入 / 幂等键）

通用约定：
- **幂等键**：合同 integrationRules（operations.json 各 async 操作均带）：「原领域
  requestId 去重字段统一由请求 envelope.idempotencyKey 提供；requestId 仅标识单次通信尝试」。
  下列各步给出建议键值（值由调用方选择，属 envelope 合同内用法，不是新字段）。
- **effect 语义**（合同 `$defs.error.effect`）：`not_applied`＝副作用前拒绝；`unknown`＝
  受理后进程中断、结果不可判；`applied`＝副作用已发生。对应 run 状态机的
  `interrupted / result_unknown`（`$defs.run.state`）。
- **查询重入**：async 操作（execution=async）受理后经 `operations.get`（查非 agent 后台作业）
  按操作/运行 id 查询终态；`receipt.durable=true` + `deduplicated` 支撑
  `cli.case.requirement.compatibility.cli.requirement.mutation-receipt`（「受理后丢响应重复
  请求只产生一次业务结果；崩溃重启仍可查」）。

### 第 0 步：停止源写入

- 性质：**前置条件，不是公开操作**。g010/a039 的措辞是「源实例停止写入后检查并备份」。
  工具侧只做验证，不代停进程（基线实例停机属用户动作；新 CLI 无权管理旧进程）。
- 验证手段：`data.inspect(dataRoot=<源根>, depth=full)`：
  - result.activeWriter：null 表示未检测到活跃写者（inspect 合同 result 字段）；
  - issues[]：`severity=error` 的条目（写锁、未知格式、损坏数据）存在即不得进入备份
    （inspect acceptance：「不默认读取真实home；检测写锁/未知格式/损坏数据并保留原文件」）。
- 实现侧活跃写者信号（基线事实，A10/A16）：`runs/registry.json.lock` 独占获取失败＝
  run 写者在；cron 目录锁被持有＝ cron 运行时在。全部锁可获取＋复扫 mtime 无变化，
  才置 activeWriter=null。具体判据在实现时以对拍装置核实（§13 T3）。
- 失败路径：activeWriter 非 null → 后续 `data.create_backup` 返回 `WRITER_ACTIVE`
  （合同 errors），effect=not_applied；`migration.stage` 同样有 `WRITER_ACTIVE`。

### 第 1 步：data.inspect（检查）

- 合同：local / readOnly；errors 含 `FORMAT_UNSUPPORTED`（未知根形态）。
- 调用：`agentcpm-pd data inspect --data-root <源根> [--depth full]`
  （绑定：`cli-v1-command-tree.json` `data inspect`）。
- 结果消费：`identity`（源根身份指纹，喂给后续 plan/stage 的 sourceIdentity）、
  `formatVersion`、`summary{sessions,artifacts,bytes}`（对账基线）、`issues[]`。
- 失败：schema 违规 → `INVALID_ARGUMENT`（not_applied）；权限 → `PERMISSION_DENIED`。
- 幂等：readOnly，`retry: backoff_read`，无幂等键。
- g010 约束：「检查不产生迁移写入」——inspect 实现不得写源根与目标根。

### 第 2 步：data.create_backup（备份）

- 合同：async / idempotentSubmit；preconditions「本机执行绑定显式 dataRoot；能获得一致性
  快照或源写者已停止」。
- 调用：`agentcpm-pd data create-backup --data-root <源根> --destination <显式新路径>
  [--include-secrets false]`。默认不导出凭据（acceptance「默认不导出凭据」；
  a038「避免把凭据意外写入不受保护目标」）。
- 结果：受理回执（receipt，operationId）；completionResult：
  `artifactId / destination / formatVersion / checksum / sourceRevision / includesSecrets /
  entryCount`（合同 completionResult 字段集，不得增删）。
- 幂等键建议：`data.create_backup:<源根规范化路径>:<destination 规范化路径>`——同键重放
  去重（`deduplicated=true`），不同 destination 用新键（`DESTINATION_EXISTS` 只保护同名目标
  不覆盖）。
- 失败语义：
  - `WRITER_ACTIVE`：effect=not_applied（备份前拒绝）；
  - `DESTINATION_EXISTS`：not_applied；
  - 进程中断（`interrupted`/`result_unknown`）：acceptance「取消不留下看似完整备份」——
    实现**先写临时文件、校验后原子发布**（acceptance 原文「先临时文件校验再发布」），
    中断只留 `.tmp`，重入用同幂等键查询/重做。
- 事件：`operation.started → backup.created → operation.completed|failed|cancelled`。

### 第 3 步：备份可读性抽查（g010「迁移前验证备份可读」）

- `data.list_backup_entries --artifact-id <id> [--cursor --limit]`（local/readOnly）：
  manifest（`formatVersion/createdAt/sourceIdentity/sourceRevision/checksum/includesSecrets/
  entryCount`）+ 有界 items（`entryId/kind/bytes/checksum`）。「分页不自动解压执行内容；
  越界条目路径拒绝」（acceptance）。
- `data.read_backup_artifact --artifact-id --entry-id --destination`（local/idempotentSubmit）：
  「校验 hash 后写入新目标，不覆盖已有文件或跳出目标root」（acceptance；
  `PATH_OUTSIDE_SCOPE`）。此步仅在需要把个别制品恢复出来核对时用，不是主流程必经。
- 用途：确认备份非空、entryCount 与 inspect.summary 对得上、抽查条目 checksum 可复算。

### 第 4 步：migration.plan（计划，preview 零写）

- 合同：local / readOnly；errors 含 `WRITER_ACTIVE`、`FORMAT_UNSUPPORTED`、
  `INVALID_DESTINATION`。
- 调用：`agentcpm-pd migration plan --source-root <源根> --target-root <目标根>
  --target-format-version <v1 格式版本>`。
- 结果：`planId / planHash / sourceIdentity / sourceRevision / targetIdentity /
  requiredBackup / changes[]{kind,count,action} / unsupportedEntries[]`。
  - `changes[].kind`：建议取值（值域合同未定，属实现约定，见 §13 T4）：
    `project / session / history-entry / run-record / schedule / skill / memory-bundle /
    credential / config-key / auxiliary-file`；`action` ∈ `convert / project-as-is /
    unsupported_projection / manual-via-<operation> / skip`。
  - `unsupportedEntries`：逐条列出**不可转换且不搬运**的旧条目（如 `model_stream_event`
    转写条目、A20 always-on 状态）——显式列出而非丢弃（migration-plan 需求用例：
    「preview零写；来源或目标变更拒绝旧plan；中断后已写项可知；不允许任意路径逃逸」）。
- 失败：源/目标相同或嵌套 → `INVALID_DESTINATION`（not_applied）。
- 幂等：readOnly 无键；同一源根可反复 plan，结果确定性（同一输入同一 plan 内容；
  planId 每次新生成或内容寻址，见 §13 T4）。
- g010/a039 约束：「规划不迁移、不自动修改连接数据」——plan 不得写源根、不得改目标。

### 第 5 步：migration.stage（暂存转换）

- 合同：async / idempotentSubmit；errors 含 `WRITER_ACTIVE`、`PLAN_STALE`、
  `INTEGRITY_ERROR`、`IO_ERROR`。
- 调用：`agentcpm-pd migration stage --plan-id <id> --plan-hash <hash>
  --backup-artifact-id <备份 artifactId> --source-revision <plan 给出的 sourceRevision>`。
  四个参数缺一不可（命令树 requiredInput）——即**必须先备份**（合同把 backupArtifactId
  设为必填，强制 g010 的「迁移前验证备份」次序）。
- 结果 completionResult：`stagingId / planHash / sourceRevision / targetFormatVersion /
  idMapArtifactId / convertedEntries / diagnostics[]{code,message}`。
  - staging 是**隔离目标**（合同标题「在隔离目标执行数据转换」；acceptance「转换仅写
    隔离 staging」）：落盘建议 `<targetRoot>/v1/staging/<stagingId>/`（内含一个 staging
    SQLite + idMap 制品 + staging manifest；路径属实现层）。**不写 `<targetRoot>/v1/state.sqlite`**。
  - `idMapArtifactId`：旧→新 ID 映射的制品引用（§5）。
  - `diagnostics`：非致命问题（如某条目部分字段不可投影）逐条记录，致命问题直接 failed。
- 失败语义：
  - `PLAN_STALE`：源版本变化拒绝旧计划（acceptance「源版本变化拒绝旧计划」）——
    stage 开始前复验 sourceRevision（对源根再算一次 identity），不一致即拒，not_applied；
  - `INTEGRITY_ERROR`：备份条目 checksum 校验失败，not_applied；
  - 中断：staging 内是纯派生数据，中断后**重入=以同 planId/planHash/sourceRevision 重新
    stage**（staging 目录按 stagingId 重建，旧半成品先由 recover(discardUnpublished) 或
    重建逻辑清掉）；acceptance「失败源数据原样保留」——源根绝不改写。
- 幂等键建议：`migration.stage:<planId>:<planHash>`（同键重放返回同一 stagingId）。
- acceptance「记录旧ID到新ID映射」在本步发生（写入 staging 的 idMap 制品）。

### 第 6 步：migration.verify（验证）

- 合同：async / idempotentSubmit；输入仅 `stagingId + planHash`。
- completionResult：`stagingId / verificationId / verified / reportArtifactId /
  checkedEntries / failures[]{entryId,code,message}`。
- 验什么（acceptance 原文「核对计数、ID映射、引用、内容hash及目标可读性」），
  §7 展开。
- 失败：`NOT_FOUND`（stagingId 不存在）、`INTEGRITY_ERROR`（staging 损坏）。
  verified=false **不是操作失败**——操作成功返回 verified=false + failures[]，
  提交被 `VERIFICATION_REQUIRED` 挡住（commit 合同 errors；g010「未验证完成不得报告
  迁移成功」）。
- 幂等键建议：`migration.verify:<stagingId>:<planHash>`；staging 只读，可重复验证。

### 第 7 步：migration.commit（发布）

- 合同：async / idempotentSubmit；errors 含 `VERIFICATION_REQUIRED`、`REVISION_CONFLICT`、
  `WRITER_ACTIVE`、`IO_ERROR`。
- 调用：`agentcpm-pd migration commit --staging-id --verification-id --plan-hash
  --expected-target-revision`。
- 前置：verificationId 必须属于**同一 stagingId 且 verified=true**（acceptance「只有匹配
  staging 的成功核验可发布」），否则 `VERIFICATION_REQUIRED`（not_applied）。
- 语义：目标并发变化（expectedTargetRevision 不符）→ `REVISION_CONFLICT`（not_applied）；
  「持有单写者锁后原子切换；不自动启动旧新双写」（acceptance）。实现建议：取得目标
  单写者锁（对 `<targetRoot>/v1/` 的 gateway 实例锁，复用 §8 的实例锁语义）→ 在一个
  SQLite 事务里把 staging 行合并进 `state.sqlite` + 写 `legacy_id_map` 表 + 递增
  `meta.revision` → 释放锁。completionResult：`stagingId / targetIdentity /
  targetRevision / published / recoveryArtifactId`（recoveryArtifactId：发布前目标
  状态的恢复材料制品 id，供第 8 步 restorePreviousTarget 用）。
- 中断重入：同幂等键（建议 `migration.commit:<stagingId>:<verificationId>`）重放 →
  查询已终态结果（mutation-receipt：「受理后丢响应重复请求只产生一次业务结果」）；
  切换点中断 → result_unknown，用 operations.get 收敛为 succeeded/failed 后再决策。
- a039 红线：「不允许新旧双写同一数据根」——commit 后工具不启动对源根的任何写。

### 第 8 步：migration.recover（恢复）

- 合同：async / idempotentSubmit；输入 `stagingId + action(discardUnpublished|
  restorePreviousTarget) + expectedTargetRevision`；errors 含 `REVISION_CONFLICT`、
  `WRITER_ACTIVE`、`RESOURCE_NOT_OWNED`。
- 两条路：
  1. **discardUnpublished**（verify 失败 / 放弃未发布暂存）：只删该 stagingId 自己的
     隔离目录与 idMap staging 制品；「恢复不删除源数据或未归属项目」（acceptance）。
     expectedTargetRevision 仍必填——若目标已被其他方改动，先要求调用方核对再操作。
  2. **restorePreviousTarget**（提交后发现问题时）：从 commit 的 recoveryArtifactId
     恢复发布前目标状态；**目标发布后有新写入则拒绝直接回滚并给出恢复步骤**
     （acceptance；REVISION_CONFLICT + completionResult.recoveryInstructions 列出人工
     步骤）。`RESOURCE_NOT_OWNED`：stagingId 不属于当前主体/实例（a039 权限语义：
     「恢复只触及该迁移拥有的资源」）。
- completionResult：`stagingId / action / recovered / targetRevision? /
  retainedSourceIdentity / recoveryInstructions[]`（源身份保留可核对——g010「中断后
  源数据与已提交目标状态可核对」）。

## 5. 旧 ID 映射规则

### 5.1 旧 ID 清单（格式与生成处见 §2 A8/A11/A15）

映射覆盖四类旧 ID：`session`（sessionKey，A8）、`run`（runId=uuid，A11）、
`task`（cron taskId=uuid，A15）、`project`（旧 projectId slug/`--sha1`，A4）。

### 5.2 新 ID

`{nodeId}-{uuidv7}`，由 `agentcpm-pd-protocol` 的 `IdGenerator(nodeId)` 生成
（`agentcpm-pd-protocol/src/id.rs:1-3,82,131-133`；007 §7.6）。迁移侧统一用一个
`nodeId`（建议实例 nodeId，默认 `local`，007 §8），使映射可按前缀识别。

### 5.3 存储与权威

- **权威 = 目标实例的 v1 SQLite**（007 §7.3「SQLite 是唯一权威」）。新增表（DDL 属
  实现层，登记进 §13 T1）：

```sql
CREATE TABLE IF NOT EXISTS legacy_id_map (
  legacy_kind  TEXT NOT NULL,        -- 'session' | 'run' | 'task' | 'project'
  legacy_scope TEXT NOT NULL,        -- 旧 projectId（project 行自身为空串）
  legacy_id    TEXT NOT NULL,        -- 旧原始 ID（sessionKey 原文，含 ':' '/'）
  legacy_storage_key TEXT NOT NULL,  -- <相对文件路径>（清洗后文件名级定位）
  new_id       TEXT NOT NULL,        -- {nodeId}-{uuidv7}
  source_revision  TEXT NOT NULL,
  migration_id TEXT NOT NULL,       -- stagingId（来源迁移）
  created_at   TEXT NOT NULL,
  PRIMARY KEY (legacy_kind, legacy_scope, legacy_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_legacy_id_map_new ON legacy_id_map(new_id);
```

- 为什么双键（legacy_id + legacy_storage_key）：A9 的清洗是 lossy 的（`/`→`-`），两个
  不同 sessionKey 可能得到同一文件名；以「存储键」定位文件、以「原始 ID」做映射主键，
  两者不一致的行进 diagnostics（`code=slug-collision`），不静默合并。
- **写入方**：只有 `migration.stage`（写 staging 内 idMap 制品，即 completionResult 的
  `idMapArtifactId`）与 `migration.commit`（把制品发布进上表）。除此无任何写入方。
- **读取方**（何时只读）：
  1. `migration.verify`：核对映射（§7）——只读 staging 制品；
  2. compat shim（`pilotdeck resume`）：只读 §5.5 的导出副本；
  3. 用户脚本：经公开操作无「按旧 ID 查新 ID」入口（285 合同查无，仅
     migration.stage/ skills.plan_import 提及 legacy），所以提供导出副本（下）。
- **导出副本**：commit 成功后，把表内容物化为
  `<AGENTCPM_PD_HOME>/compat/legacy-id-map.json`（0600）+ 同目录
  `legacy-id-map.sha256`。格式建议（内部文件，非协议）：
  `{"formatVersion":"agentcpm-pd.legacy-id-map/1","migrationId":<stagingId>,
   "sourceRevision":...,"items":[{legacyKind,legacyScope,legacyId,newId},...]}`
  与 S4-EXECUTION-PLAN §4 的 shim 目录约定（`compat/` 子目录）一致；每次 commit
  覆盖重写（commit 本身受 REVISION_CONFLICT 串行化）。
- **越权/未知旧 ID**：shim 查不到映射 → 按 sdk-resume 用例预期「未知/越权 session
  明确失败」走 `NOT_FOUND`（该入口的映射缺失不是 shim 自己编造的回退）；SQLite 侧
  无「越权」概念（单用户本机），`RESOURCE_NOT_OWNED` 用于 recover 拒绝非本主体 staging。
- **冲突论证**（两个旧 ID 撞新 ID——uuidv7 下不可能，但要写明）：
  1. 新 ID 的随机部分是 uuidv7 的 74 bit 随机（var+version 消耗后）；同 nodeId 下
     两次生成碰撞概率 ~2^-74/次；10^6 个 ID 的生日碰撞上界 ~10^14 分之几，工程上可忽略；
  2. 但**不靠概率兜底**：`idx_legacy_id_map_new` 唯一索引 + staging 写入时先查 staging
     映射内唯一性，冲突即 `INTEGRITY_ERROR`（effect=not_applied/unknown 视中断点），
     绝不静默覆盖；
  3. 反向冲突（两个旧 ID 映射同一新 ID）只会来自实现 bug 或手工改库——被 2 的约束
     转成显式失败。

## 6. 各数据类转换策略

统一原则：可转换的进 staging；**不可转换的显式列为 unsupported_projection**
（进 plan.unsupportedEntries 与 staging diagnostics，不丢弃、不静默截断）；
领域数据（skills/memory/凭据/配置）**不并入 stage 的转换产物**，在 commit 后由用户
显式经各自领域操作导入（plan 在 changes 里以 `manual-via-<operation>` 条目预告），
依据：a039「连接不自动迁移」、t020「配置运行管理由 cli.task.config 承担；本项仅负责
持久数据迁移闭环」、migration.plan acceptance「规划不迁移、不自动修改连接数据」。

| 数据类 | 旧格式（依据） | 新落点 | 可转换字段 | 不可转换（unsupported_projection） | 依赖 S3 任务/域 |
|---|---|---|---|---|---|
| 项目登记（A3/A4） | `projects/<id>/.cwd`；slug/`--sha1` ID | `project.create`（t012；projects 表） | 根路径（.cwd 原文）、名称（目录名）、environmentId=`local`（007 §9 D18） | 无（登记本身无损）；.cwd 缺失时用 resolve 规则回退并记 diagnostics | t012（project.create/update） |
| 会话（A5/A7/A8） | `chats/<safeId>.jsonl` + 尾部 session_metadata | `session.create`（t012；sessions 表） | sessionId（新 `{node}-uuidv7`，映射入表）、projectId（经映射）、name=title、tags=tag | titleSource*/aiTitle 溯源、linkedPullRequest、mode（v1 sessions 无列；保留在导出副本与报告，标 unsupported_projection） | t012 |
| 历史消息（A6） | JSONL 条目 41 种 type | 会话历史（t012 history.*；events/历史存储） | accepted_input→用户输入；assistant_message/durable_message→助手消息（content[] text 块投影，依据 chat search 的投影面 NodeSessionSearchPort.ts:219-241）；turn_result→run 终态；session_metadata→§会话字段 | model_stream_event、context_snapshot、agent_instructions、subagent_*、plan_todo_*、goal_changed、compaction_*、question_*/permission_*（交互历史）、file_snapshot_recorded、inbox_mutation、agent_status_message 等——逐条列名进 unsupportedEntries | t012（history.read/export/restore、archive.*） |
| 子代理转写（A14） | `chats/<parent>/subagents/<child>.jsonl` | 同上，按子会话归档 | 同历史消息 | 同上 | t012 |
| runs 记录（A10/A11） | `runs/registry.json` records+events | v1 runs/run_queue 表（只读导入为终态 run：state 映射 accepted→queued…completed→succeeded、aborted→cancelled、interrupted→interrupted） | runId 映射、sessionKey→新 sessionId（经映射表）、state、acceptedAt/updatedAt→created_at/updated_at | requestMaterial、result（旧 GatewayEvent 结构）——报告保留 hash，不投影 | t012 + S2 已有 runs 表 |
| 工具产物（A12/A13） | `.pilotdeck/tool-results/<safeId>/`、file-history/ | 归档制品（archive.entries 的 artifact 引用；t012） | 文件按 sha256 入制品库，条目引用新 sessionId | file-history 快照结构（v1 无对应物） | t012（archive.get/entries） |
| skills（A17） | `<PILOT_HOME>/skills/<slug>/SKILL.md` | `skills.plan_import(origin=directory, sourceRef=<显式目录>, legacyFormat=custom)` → `skills.apply_import`（preview 零写→同计划 apply） | SKILL.md 及目录内容整体 | 内置 builtin 技能（随新发行，不迁）；plugins（A18，无 v1 对应，整类 unsupported） | S3 技能域（a031）；映射用例 cli.case.compat.cli.compat.skills-migrate |
| 配置（A1） | `pilotdeck.yaml` | `configuration.read_layer/validate/update`（config 任务）人工/显式 | 可映射的模型/权限键（由 config 域的层结构决定） | gateway 特有段（adapters.*、modules、memory 路径）——逐键列出，不自动迁移 | cli.task.config |
| 渠道凭据（A2+A1 adapters） | `weixin-credentials.json`、yaml 内 appId/appSecret/token | `channels.begin_login/poll_login/configure`（S3 渠道域）+ 凭据库（credentialRef） | 无（连接不自动迁移；a039 关键语义）——plan 列 `action=manual-via-channels.*`，凭据值**不进备份**（includeSecrets=false）也不进 staging | 全部（防泄漏：迁移链路明文零接触） | S3 渠道域（a036） |
| automation/cron（A15/A16） | `cron/projects/<id>/tasks.json`、runs、run-history.jsonl | `schedules.configure`（kind=once|cron、expression、timezone、message、enabled）（S3 自动化域） | schedule（CronSchedule→kind+expression+timezone）、message、projectId、status→enabled | sessionKey/channelKey 绑定（新侧 scheduleId/sessionId 由用户重绑）；run 历史（runs/*.events.jsonl、run-history.jsonl）→ 报告保留 hash | S3 自动化域（a033）；taskId 映射进 legacy_id_map(kind=task) |
| memory（A19） | `<PILOT_HOME>/memory/` | `memory.plan_import/import`（t018，需先 export 成 bundle） | 记忆条目（经 t018 的 bundle 校验） | 无对应物/损坏 bundle → INVALID_BUNDLE 如实报 | t018（memory.export/plan_import/import） |
| always-on（A20） | `always-on/` 状态树 | 无 v1 对应操作（285 合同无 always-on 域） | 无 | 整类 unsupported（plan 列出目录与计数，不搬运） | —（后续阶段裁决，§13 T6） |
| 运行期杂项（A21） | `gateway/`、`browser_screenshots/`、`runtime/` | 无 | 无 | 整类 skip（非用户持久数据） | — |
| 旧 gateway 档案（A22） | export/native_archive 行为 | 由 data.* 操作语义承接（a038 采纳说明） | — | — | 本任务（data.* 即其承接形态） |

## 7. 验证步骤语义（migration.verify 验什么）

按 acceptance「核对计数、ID映射、引用、内容hash及目标可读性；缺失记录使 verified=false
且不能提交」拆为六项检查，全部落 `checkedEntries` 计数与 `failures[]`：

1. **计数对账**：staging `convertedEntries` == Σ plan.changes[].count（convert 类）；
   备份 manifest.entryCount == 源根文件数（inspect.summary 的 sessions/artifacts 为基线）。
   不符 → `code=count-mismatch`。
2. **ID 映射完整性**：idMap 制品中每个旧会话/任务都有新 ID；新 ID 唯一；plan 中每个
   convert 类 session 在映射中恰有一条。→ `code=id-map-missing|id-map-duplicate`。
3. **引用一致性（外键）**：staging 内 run 行引用的 sessionId 均存在；映射表 run→session
   与旧 registry 的 sessionKey→run 关系同构抽查（全量比对 run 计数 + 每会话 run 数）。
   → `code=dangling-reference`。
4. **内容 hash**：staging 每条历史/会话行的内容 sha256 与从备份条目重算的一致（备份
   entry.checksum 为准，A22 manifest 语义）；idMap 制品与报告制品自身 hash 复算。
   → `code=checksum-mismatch`。
5. **目标可读性**：staging SQLite 可开、schema 版本 == plan.targetFormatVersion、WAL 可
   写测试目录；导出副本预检（§5.5 能物化）。→ `code=staging-unreadable`。
6. **unsupported 对账**：plan.unsupportedEntries 与 staging diagnostics 的并集 ==
   实际未转换条目（防静默丢弃）。→ `code=unsupported-drift`。

verified=false 的任何一条 failures 都使 commit 返回 `VERIFICATION_REQUIRED`（合同
errors + g010「未验证完成不得报告迁移成功」）。报告全文物化为 `reportArtifactId`
制品（schema 用合同 `$defs.artifact`：artifactId/length/sha256/mediaType）。

## 8. 验证失败 → 恢复的路径（含并发模型）

- **verify 前/中失败（staging 未发布）**：
  `migration.recover --staging-id <id> --action discardUnpublished --expected-target-revision <r>`
  → 删隔离 staging；源数据原样（stage 从未碰源根）；修源或改 plan 后重新走 4-6。
- **commit 后回滚**：`restorePreviousTarget`（§4 第 8 步）；新写入存在 → REVISION_CONFLICT
  + recoveryInstructions（人工步骤），**不自动强回滚**（acceptance 原文「目标发布后新写入
  存在则拒绝直接回滚并给出恢复步骤」）。
- **并发**：
  - 源侧：stage 前复验 sourceRevision（PLAN_STALE）+ 备份锁（create_backup 的
    WRITER_ACTIVE）；
  - 目标侧：commit/recover 都要求 expectedTargetRevision（REVISION_CONFLICT），且
    commit 持单写者锁（acceptance「持有单写者锁后原子切换」）——即目标 gateway 若在
    运行会以 WRITER_ACTIVE 拒绝或要求先 `service.stop`（007 §8 服务管理）；
  - 工具内：staging 目录按 stagingId 隔离，多个迁移并行时互不覆盖；同一 stagingId 的
    stage/verify/commit 由 idempotency_records 串行化（S2 已有表）。

## 9. 与 compat shim 的衔接（同一份映射）

- 需求方：`cli.case.compat.cli.compat.sdk-resume`（C1「支持」级）：步骤首条即
  「验证旧session标识到新session映射」，预期「未知/越权session明确失败」；S4-EXECUTION-
  PLAN §2 #6 的序列是「旧会话映射 → session.get → run.submit」。
- 同一份映射、两处形态、单写多读：

| 参与方 | 对映射的访问 | 形态 | 约束 |
|---|---|---|---|
| migration.stage | **写** | staging idMap 制品 | 唯一生成点（新 ID 分配处） |
| migration.commit | **写** | v1 SQLite `legacy_id_map` + 导出 `compat/legacy-id-map.json` | 发布点 |
| migration.verify | 读 | staging 制品 | 核对 |
| compat shim（sdk-resume 等） | **只读** | `compat/legacy-id-map.json`（校验 .sha256） | 无第二后端（S4 计划 §4 决策 4：shim 不直连数据库/旧运行时）；查无 → NOT_FOUND 如实失败 |
| 用户脚本 | 只读 | 同上或 SQLite 只读副本 | — |

- 一致性时序：commit 成功（receipt succeeded）→ 同步物化导出副本（commit 的完成路径
  内完成，不留给后台任务）；shim 读文件 + 校验 hash，若文件缺/坏 → 按映射缺失处理
  （NOT_FOUND），不回退猜旧 ID。
- 未迁移会话上的 `pilotdeck resume <旧ID>`：映射无此 ID → NOT_FOUND（不带自动迁移，
  不触发迁移副作用——保持 shim 只读原则）。

## 10. 故障/权限/并发用例映射

| 步骤/机制 | 台账用例 id | 验收归属 |
|---|---|---|
| data.inspect（含写锁/未知格式/损坏数据保留原文件） | cli.case.operation.data.inspect | **本阶段主验收**（S4，操作合同草案） |
| data.create_backup（一致性拒绝、凭据默认不导出、临时文件校验、同名不覆盖、取消不留半成品） | cli.case.operation.data.create_backup | 本阶段主验收 |
| data.list_backup_entries（真备份、分页不解压、越界拒绝） | cli.case.operation.data.list_backup_entries | 本阶段主验收 |
| data.read_backup_artifact（hash 校验、不覆盖、不逃逸） | cli.case.operation.data.read_backup_artifact | 本阶段主验收 |
| migration.plan（preview 零写、源/目标根约束、规划不改连接数据） | cli.case.operation.migration.plan；cli.case.requirement.compatibility.cli.requirement.migration-plan | 本阶段主验收（两者都是 S4 阶段、a038/a039 关联） |
| migration.stage（停写+备份校验、PLAN_STALE、隔离 staging、ID 映射、失败源原样） | cli.case.operation.migration.stage；migration-plan（中断后已写项可知/路径逃逸） | 本阶段主验收 |
| migration.verify（六项核对、verified=false 阻断提交） | cli.case.operation.migration.verify | 本阶段主验收 |
| migration.commit（匹配核验才发布、并发拒绝、单写者锁原子切换、不双写） | cli.case.operation.migration.commit；cli.case.requirement.compatibility.cli.requirement.mutation-receipt | 本阶段主验收（mutation-receipt 为 S4 阶段、a039 关联） |
| migration.recover（只触及自有资源、新写入拒绝直回滚、不删源） | cli.case.operation.migration.recover | 本阶段主验收 |
| 全链端到端（备份→迁移→验证、可恢复） | cli.case.c032 数据迁移 | 本阶段主验收（S4；关联任务含 t020） |
| 迁移中断注入与恢复 | cli.case.c033 迁移中断 | 本阶段主验收（故障用例；完成条件点名「适用故障…用例」） |
| stdout/stderr/退出码机器输出 | cli.case.c034 机器输出 | 本阶段（t020 命令需符合输出合同） |
| 双平台 | cli.case.c035 双平台发行 | 本阶段（与 t021 联合；完成条件「保存版本身份」） |
| 旧会话映射消费（sdk-resume） | cli.case.compat.cli.compat.sdk-resume | **较晚阶段联合门**（cli.task.compat 的 C1 验收；依赖本任务产出的映射表，属后续集成门，不反向变成本任务前置——t020 完成条件原文） |
| 技能迁移入口 | cli.case.compat.cli.compat.skills-migrate | 联合门（S3 技能域 + compat） |
| 权限语义（每个操作的 PERMISSION_DENIED/AUTH_REQUIRED 面） | 各 operation case 的「合法/非法和故障场景」步骤 | 本阶段主验收 |

## 11. 版本身份与证据

- **凭证格式**：每次迁移的可核对链（全部为既有合同字段/制品语义，不发明协议）：
  1. inspect：`identity`（源根指纹）；
  2. 备份：`artifactId + checksum + sourceRevision + entryCount`（create_backup
     completionResult）；
  3. 计划：`planId + planHash`（planHash = changes/unsupportedEntries/身份字段的规范化
     序列化 sha256——计算法属实现约定）；
  4. 暂存：`stagingId + planHash + sourceRevision + targetFormatVersion +
     idMapArtifactId + convertedEntries`；
  5. 验证：`verificationId + reportArtifactId`（报告制品 sha256）；
  6. 提交：`targetIdentity + targetRevision + published + recoveryArtifactId`；
  7. 恢复：`recovered + retainedSourceIdentity + recoveryInstructions`。
- **谁签名/校验**：不引入外部签名体系（基线无此物，不发明）。完整性靠三段：
  - 制品自校验：`$defs.artifact` 的 `length+sha256+mediaType` 三元组（合同既有）；
  - 链式哈希：planHash→stage 记录→verify 报告 hash→commit 记录 verificationId，逐级
    引用上一级哈希；
  - 用例证据（c032–c035「证据要求」）：保存实际 argv/stdin、schema、stdout/stderr、
    退出码、前后状态、源码/产物 hash、平台、时间、环境和依赖身份；失败保留原件。
- **版本身份**：`targetFormatVersion` 承载目标格式版本；迁移工具自身的版本进
  `operations.get` 可查的操作记录与报告制品（构建版本 = CLI `--version` 输出的合同
  digest，`agentcpm-pd --version` 已含 registry digest，`agentcpm-pd-cli/src/lib.rs:314-323`）。

## 12. 实施依赖图

```
wp0-gate(已收口) ─┬─► t012 项目/会话/历史/归档 ─┐
                  ├─► t018 记忆与快照 ──────────┤
                  └─► config 配置域 ────────────┼─► t020 实现（本设计）
S3 技能域（skills.scan/plan_import/apply_import）─┘   （联合门，非进入条件）
S3 渠道域（channels.*）      —— changes 预告，非进入条件
S3 自动化域（schedules.*）   —— 同上
S3 检索域（search.*）        —— 同上（迁后历史可检索性归检索域验收）
cli.task.compat（shim）      —— 消费导出映射（sdk-resume 联合门）
cli.task.t021（发行）        —— c035 双平台联合
```

- 进入条件四人组对应关系：t012=会话/历史/归档落点与 project.create；t018=memory 导入
  通道；config=configuration.*（plan 列出的 manual 条目可执行）。
- 实现落点建议：`data.inspect/list_backup_entries/read_backup_artifact/migration.plan`
  四个 local 操作进 `agentcpm-pd-cli/src/local_ops/`（S3 substrate，`local_ops/mod.rs:1-13`
  的既定扩展方式）；五个 async 操作（create_backup/stage/verify/commit/recover）进
  `pilotdeck-gateway/src/public_v1/domains/`（需 S3 集成者开域文件）。

## 13. 未决问题（需用户 / S3 集成者裁决，不猜）

- **T1 legacy_id_map 表 DDL 与导出文件所有权**：新表 + `compat/legacy-id-map.json` 涉及
  v1 存储与 compat crate 的边界（S3 集成者管 workspace 与存储 schema）。DDL 见 §5.3
  草案，待登记。
- **T2 「双段式」迁移拆分确认**：本设计把 skills/memory/凭据/配置排除出 stage 转换、
  由 commit 后领域操作显式导入（§6 原则）。替代方案是 stage 一并转换全部领域数据。
  取舍：前者符合 a039「连接不自动迁移」与各域自有 plan/apply 语义（skills/memory 都有
  专用 preview 操作），后者少一步用户操作但把领域校验复制进 migration。**建议前者**，
  需集成者确认。
- **T3 源停写检测的权威判据**：TS 基线无单一状态文件；本设计用 registry.json.lock +
  cron 目录锁 + mtime 复扫。需在实现时以对拍装置（scripts/parity 族）注入活跃写者验证
  检测率；若不足，考虑要求用户显式确认（无合同字段可加，只能在 CLI 提示层做）。
- **T4 planId/verificationId/stagingId/artifactId 的值域**：合同只定 string；建议全部用
  `{nodeId}-{uuidv7}`（复用 IdGenerator），planHash 用内容 sha256。待实现时定并登记。
- **T5 旧 Rust gateway 的 `runs/registry.json`**：当前 Rust 版 gateway 也写该文件
  （007 §7.3「现有提交路径仍会写 runs/registry.json…registry.json 只是旧路径留下的
  副本」）。本迁移的**源定义为 TS 基线数据根**（cc7c4e6 形态，§2）；若用户源根实为
  Rust 旧版产物，形态差异（如 SQLite 缺位、runs 结构一致）是否同样支持，需产品裁决。
- **T6 always-on 数据**（A20）：285 合同无对应域。整类 unsupported 还是后续阶段补域，
  需产品裁决（影响 plan.unsupportedEntries 的表述）。
- **T7 会话标题等 v1 无落点的元数据**：title/tag 可进 session.name/tags，其余（PR 链接、
  mode、AI 标题溯源）目前只能进导出副本/报告。是否给 v1 session 加扩展列属 t012 范畴。

## 14. 给 S3 集成者的台账登记建议（仅文案，不含写台账动作）

建议在 t020 条目的「候选/下一步」记录：

> 设计稿即本文：六步闭环（inspect→backup→
> plan→stage→verify→commit/recover）全走 9 个既有操作合同，无新增协议字段；旧→新 ID
> 映射以 v1 SQLite 新表 legacy_id_map 为权威、commit 后导出 compat/legacy-id-map.json
> 供 shim 只读。待裁决：legacy_id_map DDL、领域数据（skills/memory/凭据/配置）留在
> commit 后显式导入的双段式确认、源停写检测判据。实现待 t012/t018/config 完成后排期。
