---
id: H1-MATTERMOST-NULL-FRAME-001
---

## 类别

崩溃修复：忽略Mattermost WebSocket收到的JSON null envelope或posted事件中的JSON null post，不让远端无效帧终止整个Gateway。

## 基线依据

固定cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85的`MattermostChannel.ts:130-151`在JSON.parse后直接访问属性。正式CLI server的`_tmp/20261113-h1-mattermost/null-b`和`null-c`两轮均有null-envelope/null-post两格：进程在驱动清理信号之前自主exit1，未被强杀。`null-self-compare.json`保留两轮相同结论。

## Rust 行为

`channel/mattermost.rs`对解析后的envelope/post检查可取字段再分发；null帧不提交模型、不发送回复，连接和Gateway继续服务后续有效事件。

## 范围

只包含上述两种WS null输入。`GET /users/me`返回JSON null属于普通token检查失败，Rust仍记录该错误并停止连接，不套本条豁免。其他有效帧、协议、认证和provider错误仍需精确对拍。

## 机器判据

`tests/fixtures/h1-mattermost`的null正式场景记录TS自主崩溃；Rust必须在相同无效帧后保持health成功、零模型请求，并能继续处理后续有效消息和自然退出。对应模块过滤测试与正式入口均须随固定候选执行。截至登记Rust尚未运行，19项捕获断言变异不冒充Rust实现变异或该产品成绩。

## 批准记录

用户2026-11-13执行计划明确授权崩溃修复；本条仅覆盖正式入口已复现的远端null帧导致进程退出，不给予普通Mattermost错误任何宽泛豁免。

### 2026-11-13 验证进展追加

已挂载gateway全部targets编译检查通过；Mattermost非压缩12项模块测试已12 passed/0 failed/0 ignored（`_build/20261113-045216-h1-mattermost-uncompressed-first`），包含`null_frames_are_contained_and_literal_surrogates_survive_inner_json`。压缩第13项当时未挂decoder，事先明确排除；正式Rust server null场景仍待执行，不能由模块成功代替。
