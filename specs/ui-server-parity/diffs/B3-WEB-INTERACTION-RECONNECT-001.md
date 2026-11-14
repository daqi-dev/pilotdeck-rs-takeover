---
id: B3-WEB-INTERACTION-RECONNECT-001
---

## 类别

固定TS现有网页集成的bug修复：短断连丢失旧owner binding而无法合法提交仍待回答的交互；delivered:false却假发取消确认，以及页面提前移除提示造成用户答案丢失。不是新增交互协议或改变Rust鉴权。

## 基线依据

cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85的src/gateway/client/GatewayWsClient.ts close callback先清hello再通知observer；ui/server/pilotdeck-bridge.js:256读取已空interactionBinding，跳过既有reconnect_interaction。ui/server/index.js:2575忽略elicitation helper的false仍广播permission_cancelled，permission响应分支同类。实际原桥接+Rust58的proxy4 run dde2fd27-b80a-40cf-9f53-d7881c94060b仍active，44,085ms内回答delivered:false（未超120秒）；回答前没有reconnect_interaction，随后自然timeout/completed不等于回答成功。证据_tmp/20261114-web-journey/runtime/shutdown58-evidence.json与reconnect-binding/diagnosis.md；不冒称另跑了整套TS gateway实证。

## Rust 行为

Rust58保持原owner/binding/generation拒绝逻辑不变。修复位于保留的TS网页桥接与前端，以scripts/backend-web-integration只读精确overlay加载：每gateway对象缓存首次服务端签发binding，沿原reconnect_interaction提交previous，next仍由真实新连接注入；旧Promise不得清新一代retirement。仅delivered=true广播既有permission_cancelled，composer等待此确认。runless replay按session/request合并已知身份，不覆盖冲突known run。

## 范围

仅同一UI进程中的网关WS短断连与已有交互确认、后端退出后按相关权威inactive收敛pending。UI进程重启丢失的内存owner证明不声称恢复；多代部分成功/丢回执的完整收口不声称穷尽。资源基线只读，无新API、无伪造binding、无放宽认证，无自动豁免既有B阶段评分。

## 机器判据

正式frontend19项定点控制与runtime11项控制分别覆盖scoped terminal/inactive、active/null保留、陈旧sequence/run、runless replay、未ack不移除、false不假ack、getter清空后的合法rebind与旧Promise身份保护。实际入口须看到loader两目标source/result SHA收据。正式组合的CUA question/permission短断连后delivered:true、工具真正续行及唯一terminal仍须实证；unit或静态parse不代替实际使用。原首败及控制装置首败保留。

## 批准记录

2026-11-14主协调基于用户“项目内决策你自行处理”及现有网页真实测试/优先修核心授权决定接受此精确修复，见state/decisions-20261114.md的“Web交互重连全链修复”。Astra独审报告live-interaction-rebind.md SHA d3cc663b51f2ff68773c3805d587c919503fc05e8794ac4c610c85f38661f311。不是用户逐字批准具体实现。仅登记授权预期，实际复验未完成前不标通过，不改旧冻结文件、分母或比较器。
