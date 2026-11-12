# SDK directory MCP 正式生命周期候选

独立场景 `sdk-directory-plugin-mcp.json`，不修改已验 directory 三能力场景。插件只放在 `PILOT_HOME/sdk-directory-mcp`，通过正式 WS `submit_turn.sdkSessionConfig.plugins` 加载；不配置 user/project 发现目录或全局 mcp.json。

固定 TS 源码依据：`InProcessGateway.ts:1085-1098` SDK 配置变化标记 session dirty；`SessionMcpRuntimeBundle.ts:54,89-125` 持有 shared/per-session runtime 资源；`SessionMcpRuntimeRegistry.ts:43-57` dispose 返回 release promise；`McpRuntime.ts:110` await clients.close。实际已安装 MCP SDK `client/stdio.js:137` close 先 EOF 等待 2 秒，再 SIGTERM 等待 2 秒，最后必要时 SIGKILL，没有最终 await。因此此候选只覆盖可信、无后代、响应 SIGTERM 的 stdio 实例，不能证明任意恶意进程或后代完全回收。

A initialize/list/call 必须同时进入真实服务端 journal、模型工具声明和调用结果。独立 session 无该工具。B 换代工具调用读取 A 的固定身份记录，执行 `kill(pid,0)` 存活探测，只在旧 PID 已消失时返回 `previousAlive:false`。`{}` clear 后实际 builtin bash 运行独立 Node 探测，再次探测 A/B，结果进入下一真实模型请求。探测不发送终止信号。PID 文件与启动 journal 具有 generation identity token，strict 校验关联；PID 被复用视为仍存活并失败，不误报成功。A/B PID 相同也拒绝。不是仅观察 gateway 最终退出或最终目录。

可信服务器 EOF 后保持 event loop，等待关闭器 SIGTERM；这把未 await close 的缺陷放大为下一阶段可观察存活，不以测试 sleep 决定通过。跨实现不要求 EOF/信号细节相同，不记录这些细节。30 秒只是每正式 WS 请求的失败上限。

运行：`node scripts/bbx/fixtures/sdk-directory-plugin-mcp/verify.mjs CAPTURE --self-test`。需要 E0 在隔离 HOME/PILOT_HOME 的固定 TS 正式入口先校准；本地未启动 BBX/gateway，也没有产品 Cargo。当前 verifier 尚未经过真实 capture 校准，不能报告正式通过。

首轮后仍需实际故障变体：MCP call 返回不同 marker，或 B 故意报告 previousAlive:true；真实 capture 成功但 strict 必须拒绝。`--self-test` 的 6 个结构 corruption 只是检查 verifier 防空结果，不替代真实进程负控。清理边界仅 POSIX，Windows 未承诺。
