---
id: B3-WEB-SESSION-OWNER-001
---

## 类别

固定 TS 单用户网页会话归属 bug 修复。权限响应把其他 session 的 requestId 当作本会话请求消费，可能执行未经该会话操作授权的文件副作用；编辑请求的替换目标与实际执行目标不一致，可能修改错误会话。这属于安全与数据归属错误，不是新增产品功能。问题回答与权限响应共享同型遍历归属缺陷，按同一精确 session 约束修复；问题回答目前只有源码及受控公开函数证据，不冒称已跑真实网页错误归属。

## 基线依据

固定基线为 cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85。ui/server/pilotdeck-bridge.js:2752 起的 decidePermissionViaGateway 与 :3854 的 elicitationRespondViaGateway 遍历 sessionState，未按来帧 session 限定请求归属。ui/server/index.js:2511 起的 regenerate-last-message 以顶层 sessionId 优先选替换目标；ui/server/services/regenerateLastMessage.js:43 准备该目标的替换，却在 :75 起把原 options 交给 runChat；bridge :1916 从 options.sessionId 或 sessionKey 选择执行会话。

实际首败使用 backend61 与 Web12 的现有网页 WS 入口和隔离合成 provider，未调用真实模型。权限首败保留在 _tmp/20261114-web-full-integration/security/owner-first/：wire.jsonl、pending.json、files-before.json、files-after-wrong-owner.json 与 provider-sec-original.json。B 发来含 A requestId 的 allow，A 文件实际被创建，provider 真实收到 Created security-a.txt. 工具结果。后续合法 deny 的收尾另记，不把收尾当作首次拒绝。

编辑首败保留在 _tmp/20261114-web-full-integration/sessions/edit-owner-first/ 的 mismatched-request.json、wire.jsonl、before.json、after.json、result.json 与 durable-followup.json。顶层 A、options B、expected A 的请求得到归 A 的 success:true；A 原19行字节保留，B 从19行增至36行，新增独特 S01_OWNERWRONG 的 accepted_input、模型请求和成功 turn_result；provider 请求增加1次。A 未丢原历史不抵消 B 被误执行的失败。以上为基线桥接接 Rust 的实际入口证据，不称独立 TS 全栈重跑。

## Rust 行为

Rust gateway 的 session/request/binding 归属校验保持不变。修改位于保留 TS UI server 的精确 runtime overlay：scripts/backend-web-integration/runtime/overlay.mjs:79 的 ownerIndex、:83 的 ownerBridge 以及 :99 起的 transformEditIndex。

permission-response 显式向 bridge 传来源 session；elicitation-response 同样传来源 session。bridge 缺失、非法或未知 session 时返回 false，不发 Gateway 请求；已知 session 仅向该 session 调用一次，不再搜索其他 session。仅 delivered=true 沿用既有成功确认。

regenerate-last-message 在 watch 与 replacement prepare 前检查顶层 sessionId、options.sessionId、options.sessionKey 的所有显式非空身份。任一归一化后矛盾则发既有 regenerate-last-message-result 的 success:false，不准备、执行或回滚其他会话。无冲突才将执行 options.sessionId 归一为已解析 owner，使 prepare、runChat、finalize 使用同一归属；仅顶层或仅 options.sessionId 的合法旧入口保留。原事务在输入持久接受前失败应回滚、接受后不得回滚的行为不变。

## 范围

限同一现有单用户 UI server 中的 permission/question 精确 session 归属，以及 regenerate 的身份一致性。不是跨用户鉴权修复证明，不扩展 no-login/auth-required 规则、token、WS 连接身份、项目访问或任意其他方法合同。不是允许客户端通过任意 sessionId 获得访问；既有 Gateway 归属和 binding 校验继续生效。

不改变正常同 owner 操作、旧 ACK schema、错误外壳、Rust 幂等或持久格式，不删除错误历史或恢复首败会话。冲突明确拒绝，不能悄悄覆盖相互矛盾的 ID 后继续执行。缺失与未知 owner 的 permission/question 不遍历兜底。未建立“顶层完全缺失时仅 options.sessionKey 自动补入口”的新合同。

## 机器判据

scripts/backend-web-integration/runtime/controls.test.mjs 中新增 decidePermissionViaGateway 与 elicitationRespondViaGateway 的缺失、未知、错误 owner 单次调用、正确 owner 控制；原 baseline helper 作为负控，证明会消费不同显式 owner 的请求。新增 regenerate owner 控制覆盖 top/options 冲突、缺失顶层但 options 两别名冲突、top/sessionKey 冲突、same owner、legacy top only、legacy options only；冲突必须零 watch、零事务调用。原 index 负控必须抓到 prepare A / execution B。

正式命令：node scripts/backend-web-integration/runtime/verify.mjs --baseline-root /Users/da/ws/pilotdeck-rs/resources/PilotDeck --output <新的隔离证据目录>。本次实际收据 _tmp/20261114-web-full-integration/sessions/formal-runtime-verify-first/receipt.json 的 SHA256 为 1c188f2b978781a4d0194a956996cf61dbe984af26956350c4830d5a673b7494；22项控制、modelProbeControls 和 parseWithTsxHooks 通过。source SHA、前一 bridge/index 结果 SHA 与最终结果 SHA 分别固定，未改变 loader 目标集合。

这些是变换后公开 helper/WS 分支的受控验证，不能代替实际修复后的 WS/provider/副作用/持久链、权威活动状态或 root CUA。实际修复组合验证待 root；本条不登记实际网页 PASS。私有事务函数9控制只解释原事务合同，最终部署使用 index guard，不把它冒作已部署 service 变换。

## 批准记录

2026-11-14 root 根据用户本轮“项目内决策你自行处理”及继续完整联调、修复已证缺陷的授权，自主接受上述精确 bug 修复并授权正式应用四文件；本条为已接受差异方向，实际修复验证待 root。项目内决策与网页必要修复授权背景见 state/decisions-20261114.md:177、:203；具体两个首败、Astra 关键逻辑及组合审、正式投影与收据见 _tmp/20261114-web-full-integration/sessions/formal-delivery.json 及上述原件。不是用户逐字审批每行实现，不外推跨用户安全保证。

本条只新增文件，不改 accepted-diffs.md、FROZEN.md、判分器、比较器、阈值、分母或已有豁免。批准修复方向与受控验证不等于实际网页验收完成。
