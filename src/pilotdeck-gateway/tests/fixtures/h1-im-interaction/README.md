# H1 共享 IM 回应端口

固定基线 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。
`generate.mjs` 直接加载未修改的 TypeScript `ImElicitationHelper`、
`ImPermissionHelper` 和 `GatewayInteractionCoordinator`；真实 pending buses
接收注册、回答和取消，不以假的 delivered 常量代替响应。源文件 SHA-256
保存在 `oracle.json`。2026-11-13 两轮输出完全相同，原始产物在
`_tmp/20261113-h1-im-interaction/baseline-{a,b}.json`。

11 格覆盖 UTF-16 孤立代理项、用户取消补充信息、缺 requestId、允许一次、
允许本会话、拒绝、外来会话、turn 取消后的迟到答案、同 chat ID 的两独立
helper、排队权限提示的发送失败重试。Rust 使用真实 coordinator 及新原生
responder，检查回调结果、pending 数量、helper 状态和显示文本。
`h1_native_im_oracle_detects_wrong_permission_decision` 故意把拒绝改成允许，
要求同一比较器发现差异。额外测试覆盖 responder 错误与 future 取消后的重试。

隔离运行示例（`BASELINE` 为固定只读目录，`PROBE` 为本任务临时目录）：

```sh
HOME="$PROBE/home" PILOT_HOME="$PROBE/pilot" \
node --import "$BASELINE/node_modules/tsx/dist/loader.mjs" \
  src/pilotdeck-gateway/tests/fixtures/h1-im-interaction/generate.mjs \
  "$BASELINE" "$PROBE/oracle.json"
```

本装置验证共享 helper → 生产回应端口 → Gateway pending bus 的完整链路；
不代表渠道的 HTTP/WS 入口、真实模型 turn 或停止后排空已经验收。
这些入口由各渠道的 H1 正式 server 场景和 owned submit 取消测试单独验证。
