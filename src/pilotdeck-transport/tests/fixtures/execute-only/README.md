# execute-only sidecar 观察装置

每个脚本从真实 TS 或 Rust sidecar 启动临时 loopback TCP，宿主声明 capability 仅有 `execute`；模型按确定性返回工具调用。参数：`node <脚本> ts|rust <真实sidecar入口绝对路径> <不存在的输出目录>`。每次只使用输出目录下隔离 home；结果是完整原帧，不把脚本退出 0 当产品通过。固定TS身份 `cc7c4e653b15c8b2cec7bcf2d15d69396ae85`。

single：一次模型工具调用、host success、再次模型、completed。concurrent：两个安全工具均到达后才倒序回应，第三个非安全工具必须在两者之后派发，最终工具结果保原输入顺序。failure：并发组一项返回真实非OK，按该项error保留，其他工具与后续串行调用继续。cancel：两项均已派发但未回应时发真实cancel，串行项不得派发。产物需用独立严格核对器逐条核对输入身份、帧序和终态；probe不是通过报告。

TS 单工具 Linux Docker 与 Rust 3f封存首败：`_tmp/20261113-e0-resume/sidecar-execute-only-{ts,rust-3f}`。TS多工具实际观察位于 `_tmp/20261113-core-final-audit/multitool-ts-{second,failure,cancel}`，仅单轮开发取证。首次single mock因回复缺content而failed，原始输出曾被覆盖，不记双轮。测试应在最终源码及binary固定后重跑，新增strict validator和故障负控。
