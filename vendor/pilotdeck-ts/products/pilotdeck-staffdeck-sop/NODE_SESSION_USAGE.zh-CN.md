# SOP 节点会话使用说明

SOP 默认使用 `new_session`：每个节点有自己的持久内部会话。同一节点等待用户补充信息、审批恢复或进程重启后，继续复用该节点的会话；切换节点时使用目标节点的会话。用户始终在同一个主会话里交流。

部署步骤见 [部署说明](DEPLOYMENT.zh-CN.md)，测试范围和验收结果见 [节点会话验收](NODE_SESSION_ACCEPTANCE.md)。

## 选择模式

| `contextMode` | 节点执行时的上下文 | 适用场景 |
| --- | --- | --- |
| `new_session`（默认） | 独立节点会话；首次进入时带入当前用户输入、节点指令和已保存的 SOP slots | 节点职责独立，避免旧对话和工具输出干扰 |
| `inherit` | 使用主会话身份，继承前序有效 messages，包括压缩后的摘要 | 后续节点需要直接继续前面的分析或工具对话 |

`new_session` 不复制前一节点的历史 user、assistant、tool messages。它不是每条用户消息都新建会话，也不表示启动独立进程。会话模式本身不选择模型；节点的 `model` 可独立配置。

## 配置模块默认值

在现有 module profile 中加入 `contextMode`。下面是配置片段，需要与已有模型和其他模块配置合并：

```yaml
modules:
  sop:
    enabled: true
    provider: staffdeck
    endpoint: http://sop-runtime:8091
    definitionsPath: ../sops/operator-approval.yaml
    defaultSopId: operator_approval
    contextMode: new_session
    timeoutMs: 10000
```

省略 `contextMode` 时仍为 `new_session`。要恢复原来继承历史的行为，将它改为 `inherit`，并检查节点和远程 owner 是否设置了更高优先级的覆盖值。

随产品提供的 [profile](profiles/staffdeck-sop.yaml) 已显式配置 `new_session`。

## 配置单个节点

本地 SOP 定义中的 `contextMode` 放在 `content.nodes` 的节点对象上。例如，收集信息节点独立运行，确认节点继承前序上下文：

```yaml
sops:
  - id: onboarding
    version: "1"
    name: 入职信息确认
    content:
      start_node_id: collect_profile
      nodes:
        - node_id: collect_profile
          contextMode: new_session
          instruction: 收集用户姓名，将姓名保存到 name slot 后完成本节点。
          expected_user_info: [name]
          allowed_actions: [ask_user]
        - node_id: complete
          contextMode: inherit
          instruction: 根据已保存的 name slot 确认信息收集完成。
      edges:
        - source_node_id: collect_profile
          next_node_id: complete
      terminal_node_ids: [complete]
```

使用此定义时，将 `definitionsPath` 指向保存的 YAML 文件，并将 `defaultSopId` 设为 `onboarding`。模块默认 `inherit` 时，也可以在某个节点上设置 `new_session`，只隔离该节点。

### 为节点选择模型

在节点上增加 `model: provider/model` 即可指定该节点使用的模型。模型标识必须对应当前 PilotDeck 模型目录中的 provider 和 model：

```yaml
content:
  nodes:
    - node_id: classify
      model: openai/gpt-4.1-mini
      contextMode: new_session
      instruction: 判断请求类型并保存分类结果。
    - node_id: summarize
      model: anthropic/claude-sonnet
      contextMode: inherit
      instruction: 根据前序上下文生成摘要。
```

节点模型只覆盖当前节点的 provider/model，模型不存在、provider 不可用或不满足 Gateway 的模型策略时，该节点在模型调用前失败，SOP 状态不会推进。节点没有 `model` 时，继续使用本轮已有模型选择和 Router 行为。

格式错误会报告 `provider/model` 诊断；目录缺项分别报告 `provider_not_found` 或 `model_not_found`。策略拒绝沿用现有 Gateway/Router 错误码，例如 `MODEL_POLICY_DENIED`、`GATEWAY_ORGANIZATION_MODEL_DENIED`、`SDK_MANAGED_MODEL_DENIED`。恢复已有会话时也会重新检查当前目录和策略，但不会替换快照中的节点模型。

节点 YAML 模型优先于用户本轮的模型选择；它不改变 `speed`、`thinking` 等未在节点 YAML 中配置的 turn 参数。模型选择只由本地定义提供，远程 owner 的 `prepare.step` 不会覆盖它。

### 会话模式优先级

最终会话模式按以下顺序决定，取第一个明确设置的值：

1. 远程 SOP owner 的 `prepare` 响应中的 `step.contextMode`。
2. 本地匹配节点的 `contextMode`。
3. `modules.sop.contextMode`。
4. 运行时默认值 `new_session`。

远程 owner 使用相同字段和取值，例如在返回的 `step` 对象中加入 `"contextMode": "new_session"`。字段仅接受 `new_session` 和 `inherit`；非法值会被拒绝，非法 owner 值不会进入持久 SOP 状态。

## 跨节点传递业务数据

需要后续节点使用的数据应写入 SOP 的 `slotUpdates`。不要依赖后续独立节点能读取前一节点的工具 transcript。

例如，节点 A 查到账号后，模型通过 `submit_step_result` 提交的结果可以包含：

```json
{
  "status": "completed",
  "replyFragment": "已确认账号信息。",
  "slotUpdates": {
    "accountId": "acct-123",
    "name": "Ada"
  },
  "nextStepId": "confirm_account"
}
```

这是节点结果示意，工具由 AgentLoop 调用，普通用户无需手动调用。SOP 图必须允许转到 `confirm_account`，状态变更和跳转由 owner 校验。节点 B 使用 `new_session` 时仍能获得 owner 提供的已知 slots，但不会获得节点 A 的完整工具输出。需要保留的工具结果应提取为 slots。

## 实际运行流程

假设主会话为 `S`，流程为 `A -> B -> C`，三个节点都配置 `new_session`：

1. 用户在 `S` 发消息，节点 A 创建内部会话 `N-A`。如果缺字段，A 返回 `awaiting_user`。
2. 用户继续在 `S` 补充字段，输入进入已有 `N-A`。A 完成后提交 slots，owner 推进到 B。
3. B 使用独立会话 `N-B`，接收当前用户输入、B 的指令和已知 slots。可执行节点可以在同一轮内连续推进，用户不必每次手动触发下一个节点。
4. B 若进入审批等待，保存主会话的 wait 和 revision。审批恢复后继续使用 `N-B`。
5. 切换到 C 时使用 `N-C`，最终回复仍显示在 `S`。

内部会话身份由“主会话 + SOP + 节点”稳定生成，不包含 `turnId`。重试、工具错误和协议纠正不会因此新建额外节点会话。再次进入同一节点会复用该节点已有会话。

结束状态的处理：

| 状态 | 后续操作 |
| --- | --- |
| `awaiting_user` | 在同一主会话发送普通消息补充信息 |
| `handoff` / 外部等待 | 先接受对应 wait 的恢复请求，再在同一主会话提交恢复消息 |
| `terminal` | SOP 完成，最终回复归属主会话 |
| `blocked` | 当前 SOP 已终止；后续普通聊天不会自动重跑它 |

## 审批与外部任务恢复

所有 status、resume 和普通聊天请求都使用主会话标识，不使用内部节点会话标识。

Web API 可通过以下地址读取状态：

```text
GET /api/sop/status?sessionKey=<主会话>&projectKey=<项目>
```

响应为 `{ "status": ... }`。从状态中读取当前 `wait.id` 和 revision，再提交恢复请求。以下请求体是示意；会话、项目、wait 和 revision 应替换为当前状态的实际值：

```text
POST /api/sop/resume
Content-Type: application/json
x-staffdeck-approver-authorization: Bearer <审批人的 StaffDeck token>
```

```json
{
  "sessionKey": "<主会话>",
  "projectKey": "<项目>",
  "requestId": "approval-request-001",
  "waitId": "<当前 wait.id>",
  "source": "human",
  "message": "审批通过。",
  "expectedRevision": 7,
  "slotUpdates": { "approved": true }
}
```

人工审批需要已登录的 PilotDeck 用户和对应的真实 StaffDeck 审批人身份。直接调用 Gateway 时，审批凭证字段为 `approverAuthorization`。外部任务恢复使用 `source: external_task`，并遵循其等待绑定要求。

恢复是两步操作：先调用 resume 接受审批或任务结果，再将成功响应中的 `message` 作为普通聊天消息提交到同一个主会话。resume 本身不会运行 AgentLoop；已有 UI/宿主集成应负责第二步。

重试同一请求时保持 `requestId` 和请求内容不变，重复请求会被去重。过期 wait、错误 revision 或错误主会话会被拒绝，不推进 SOP 状态。

## 会话归属与重启

| 内容 | `new_session` 下的归属 |
| --- | --- |
| 普通会话列表、用户可见对话、最终回复、Gateway 对外事件 | 主会话 |
| SOP 状态、revision、wait、resume、回复投递 | 主会话 |
| 路由 sticky state、汇总 usage、turn/task 预算和 `maxTurns` 限额 | 主会话 / 外层执行，节点切换不重置限额 |
| 节点模型与工具 transcript、Context cache、memory capture、compaction | 内部节点会话 |

内部节点会话不会出现在普通会话列表中，也不会把完整内部 transcript 重复写入用户可见对话。重启时应保留主会话的 SOP 持久状态和内部节点会话记录，恢复后才能继续复用当前节点上下文和 wait。

SOP 定义按主会话保存快照。修改节点的 `model` 不会重写已有会话的定义或内部节点 transcript；验证修改后的节点模型应新建主会话。修改模块配置后，需要重载到新的运行时实例或重启服务。

## 常见问题

| 现象 | 检查方式 |
| --- | --- |
| 后续节点不知道前一节点查到的数据 | 检查结果是否写入 `slotUpdates`，以及 owner 是否保存并提供该 slot |
| 节点模型没有生效 | 检查模型是否写在匹配的 `content.nodes[].model` 上，并确认该模型已加入当前 profile 的 `model.providers.<provider>.models` |
| 设置模块 `inherit` 后某个节点仍独立运行 | 检查 owner `step.contextMode` 和本地节点覆盖值 |
| 会话列表里找不到节点会话 | 内部会话默认隐藏，用户继续使用主会话即可 |
| 改完定义后旧会话行为不变 | 旧会话使用定义快照；用新主会话验证 |
| resume 成功后没有继续执行 | 检查是否把响应 `message` 提交为同一主会话的普通聊天消息 |
| resume 被拒绝 | 从主会话重新读取 wait 和 revision，确认请求属于当前等待，并使用对应审批人身份 |
