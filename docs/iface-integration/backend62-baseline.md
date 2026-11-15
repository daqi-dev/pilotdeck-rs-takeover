# Backend62 基线与验收索引

- 原始后端候选：`33b23a773b437aaea33696292810cf67aa54fdef`。
- 固定 TypeScript 行为基线：`OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。
- 本提交仅新增本说明；`src` 的树为 `8fd385fa690b4ec6128bba21272825921cc77db9`。
- backend62 历史交接报告：macOS 和 Linux 各 273 个受影响测试通过，fmt、clippy、doc、release 通过；并非在 backend62 上重跑整个 workspace。历史记录可能引用未随本分支发布的本机证据，不能把该索引本身当成独立测试收据。
- Linux 可选 browser 插件仍待验证；full 安装体积目标未达，Linux 性能未测。旧测试结果不证明 ifACE 接入已经通过。
- 完整历史安全检查的 134 条 gitleaks 命中经核对为四个测试和证据登记文件：132 条文件 SHA-256，2 条本地测试 WebSocket 握手字段；均保留原始文件，未更改历史或静默消除告警。
- 本分支不包含用户本地凭据、构建产物或正在开发的 ifACE 工作区改动。新的构建和验收须另记真实执行版本及证据。
