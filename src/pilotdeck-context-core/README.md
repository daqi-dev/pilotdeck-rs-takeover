# PilotDeck Context Core

从 backend62 的 Gateway 上下文实现中提取的纯算法包，以固定 TS 基线 `cc7c4e6` 的既有行为为准。运行时仅依赖 `pilotdeck-protocol`，不依赖 Gateway、Kernel、Tokio、模型、文件系统或权限服务。

包含：

- Snip 中段裁剪、检查点和最新用户请求保护。
- Micro 旧工具结果压缩与保护工具判断。
- 工具调用与结果成对清理，含媒体引用及 UTF-16 工具 ID。
- Token 预算快照、输出保留量和自动压缩阈值决策。
- 两端共用的 canonical wire-message 与 ECMAScript 空白字符处理。

`SnipEngine::snip` 显式接收消息 token 计数函数；`MicroCompactionEngine::apply` 接收文本 token 计数与工具结果文本投影函数。错误类型由调用方决定，并原样返回。算法不创建另一套 tokenizer 或 token 统计。Gateway 的旧公开路径保留薄适配，注入已有 `model_cost`、`pilotdeck-tokenizer` 和工具结果投影实现，因此原有调用者与测试入口保持不变。

`budget::snapshot_from_tokens` 接收 token 数、上下文容量、metadata、warning/blocking 阈值，保留原有 NaN、Infinity、字段缺失以及 usage 来源语义。它只计算结果，不持有、扣减或落盘预算账本。

以下仍属于宿主编排，不放进本包：模型驱动摘要、远程 token count、HTTP/cache、压缩日志持久化、文件溢出、会话状态和恢复决策的执行。提取纯算法不代表这些能力已通过 ifACE 正式接入。

验证入口：

```sh
cargo test -p pilotdeck-context-core
cargo test -p pilotdeck-gateway --test compaction_snip_micro
```

独立 consumer 测试检查注入边界、错误传播、预算来源、工具成对关系、保护和 UTF-16。原 Gateway 的 `compaction_snip_micro` 测试继续使用原来的固定 TS oracle，验证正式适配入口的逐字节行为；oracle 未改写。
