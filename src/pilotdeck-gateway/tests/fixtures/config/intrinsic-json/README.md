# 固定 TS 纯 JSON intrinsic 键矩阵

基线 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d69396ae85`，Node v22.22.0；调用固定 `dist` 的真实 `loadPilotConfig`，每格独立 Node 进程和临时 HOME/PILOT_HOME，不启动服务。覆盖 `Object.prototype` 12 个键分别用于 provider、model（同时作为 agent selection）、alwaysOn 顶层键、alwaysOn 项目内键，共 48 格。输出保留完整配置 model、agent、alwaysOn 与 diagnostics，provider 失败保留真实 TypeError。两轮录制逐字节一致。

在仓根运行 `node src/pilotdeck-gateway/tests/fixtures/config/intrinsic-json/record.mjs /tmp/recorded.json`，再运行 `node src/pilotdeck-gateway/tests/fixtures/config/intrinsic-json/check.mjs src/pilotdeck-gateway/tests/fixtures/config/intrinsic-json/oracle.json /tmp/recorded.json`。不得用真实 HOME。故障负控将 model/constructor 的成功改成失败后，checker 非零退出；这不等于 Rust 产品测试已通过。

`__proto__` 的构造/序列化和其他键不同，结果不可合并。该矩阵仅证明单次 loader 的记录，不证明 JS 全局 intrinsic 跨 RPC 副作用。旧 `host-boundaries.json` 的 18 格保留真实 load 与 wire 原始记录，另由 Rust 测试逐条对拍。
