# SDK 输出与子代理事件开关

任务基线：`OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。实施起点：根封存 `4525bb2abdee1e7e1f398889d9ad56baa3337367` 后解冻。状态：进行中；新增Rust测试尚未编译，不是已交付。

本子包独占 `native_process/sdk_output_controls_tests.rs`、`tests/fixtures/h1-sdk-event-controls/*` 及本文；只在 `native_process/tests.rs` 追加对应测试子模块声明。`sdk_config.rs` 的gate与会话配置映射由H1 email作者唯一修改，子包未并发编辑。

## 固定行为

- `src/cli/SessionAgentConfigBundle.ts:132`：只有 `agentProgressSummaries === true` 才设置 `includeToolProgress:true`，false/absent均不设置。`src/agent/loop/AgentLoop.ts:2770-2788` 通过tool context progress回调发 `tool_progress`；不是新增一轮模型摘要请求。
- `SessionAgentConfigBundle.ts:140`：outputFormat存在时设置 `stopOnStructuredOutput:true`。`src/agent/loop/AgentLoop.ts:2192-2208` 在structured_output工具输出后完成当前轮。schema通过Gateway的JSON-schema子集验证/存储；固定源码没有将该schema作为provider response_format的消费点，不能新增provider schema强制行为冒充迁移。
- `src/gateway/client/InProcessGateway.ts:1674` 从本次input.sdkSessionConfig取 `forwardSubagentText === true`。`GatewayAgentEventProjector.ts:539-567`：true只将子代理text提升为独立 `subagent_text_delta`；false为 `agent_status/event=subagent_text_delta/detail`；thinking在两种情况下都保持status。

Rust现有真实能力位于 `runtime/turn/submit.rs` 的forward参数投影、`runtime/turn/projector.rs` 的事件映射、`host/modules/tool_context.rs` 的includeToolProgress回调，以及 `pilotdeck-transport/factory_payload.rs` 和 `pilotdeck-agent/turn.rs` 的stop_on_structured_output解析/完成分支。本轮只补被错误拒绝的SDK gate与缺少的两flag映射，不重写已有能力。

## TS 取证与负控

新增 `tests/fixtures/h1-sdk-event-controls/oracle.cjs` 对六个完整固定TS模块做内存转换；作用域外import若被访问则fail closed。permission context使用原版pure函数，没有复制或重写被测业务。模块源码SHA逐个记录。始终隔离HOME/PILOT_HOME，无真实provider或账号。

根授权轻量槽2后两次运行并cmp一致，证据：

- `_tmp/20261113-sdk-output-controls/oracle-{first,second}.json`
- `_tmp/20261113-sdk-output-controls/identity.json`

包括五格配置compose、四格forward×text/thinking投影、structured_output→turn_completed顺序和tool_progress投影。该TS特征证明配置/事件语义，不代替Rust实际循环验收。

静态校验器 `verify.py` 检查完整场景集合与具体值。首版误把六个load计为七个，正控失败；记录 `validator-first-failure.txt` 后只改装置计数，正控PASS。随后把outputFormat那格的stop标记从true改为null，实际校验器exit1，日志 `negative-stop.log`。未修改原oracle，未把第一次装置失败抹去。

```sh
HOME="$PWD/_tmp/20261113-sdk-output-controls/home" PILOT_HOME="$PWD/_tmp/20261113-sdk-output-controls/home/pilot" node src/pilotdeck-gateway/tests/fixtures/h1-sdk-event-controls/oracle.cjs
python3 -I src/pilotdeck-gateway/tests/fixtures/h1-sdk-event-controls/verify.py _tmp/20261113-sdk-output-controls/oracle-first.json
```

## Rust 真实执行测试（待运行）

`native_process::tests::sdk_output_controls_tests` 有三项，使用正式 `LocalGateway`、本地HTTP模型、原生工具/factory及完整submit→事件投影：

1. `sdk_output_format_drives_real_structured_tool_termination`：实际structured_output工具，启用时主模型1次、未启用时2次；校验payload、完成事件及没有发明provider response_format。
2. `sdk_progress_flag_gates_real_tool_context_emission`：实际Rust扩展工具始终执行一次；只有true才得到progress回调并产生tool_progress。
3. `sdk_forward_subagent_flag_routes_trusted_child_events_through_submit`：可信Rust factory发子代理原始text/thinking事件，经正式submit按本次SDK配置路由；并未将此测试声称为完整嵌套子代理模型验收。

同一可信mock `ControlProvider` 在native测试范围公开 `start_with_arguments`，供H1 email的FileChanged生产write测试复用，不复制多份模型装置。

建议统一Cargo槽运行：

```sh
cd src
cargo test -p pilotdeck-gateway --lib native_process::tests::sdk_output_controls_tests
```

作者未运行Cargo。`rustfmt`通过；合入后的编译、三项测试、正式SDK入口组合及完整子代理模型场景仍待根验证。不能仅因gate不再拒绝就标记输出控制完成。
