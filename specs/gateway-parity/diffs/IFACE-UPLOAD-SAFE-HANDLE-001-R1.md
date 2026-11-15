---
id: IFACE-UPLOAD-SAFE-HANDLE-001-R1
original: IFACE-UPLOAD-SAFE-HANDLE-001
---

## 类别

安全修复补充。原条目只按 descriptor stat 长度精确读取，能拒绝持续追加，却仍会为预先存在的恶意巨大普通 JSON 文件按其逻辑长度增长 Vec，存在内存耗尽风险。本更正对 JSON 控制记录增加独立安全预算，不改变附件二进制的可配置大小合同。

## 基线依据

固定 TS `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的 `src/gateway/dialog/UploadStore.ts:384` 与 `:433` 使用 Node `readFile` 读取 metadata／lease JSON，没有应用大小预算。原条目的实际 FIFO／正常记录基线探针继续保留；本补充不为证明内存耗尽而实际读取巨大文件，也不冒称已经运行该攻击的 TS 测试。

Rust 现有 Node 错误映射 `src/pilotdeck-gateway/src/node_io/generated.rs` 已把 `libc::EFBIG` 映射为 `EFBIG`／`file too large`，本修改复用该既有诊断规则，并保留 read/write 操作名，没有新造 JSON 错误结构。

## Rust 行为

`src/pilotdeck-gateway/src/upload/mod.rs` 的 `MAX_JSON_METADATA_BYTES` 固定为 `64 * 1024 * 1024`；metadata.json 与 lease.json 共用同一预算。`read_exact_file` 在创建 Vec 和第一次读取前校验由同一已打开 fd 取得的长度，超限返回 `EFBIG: file too large, read`；限制以内仍通过同一精确读取函数拒绝短读和 stat 后增长。

metadata 与 lease JSON 写侧在序列化后、打开临时写文件前校验同一预算，超限返回 `EFBIG: file too large, write`，避免产生随后按本合同读不回的记录。既有文件不会被超限写入替换。

cleanup 沿用原来的两种处置：不可读上传记录记录诊断并跳过，不猜测删除；不可读 lease 元数据按原过期孤儿策略回收。有效且未来到期的 lease 在同轮保留。

## 范围

只限制上传控制面的 JSON 字节数（含序列化末尾换行），包括 metadata.json 与 lease.json；恰好 64 MiB 允许，64 MiB + 1 拒绝。附件二进制不经过此函数，不增加 1 GiB 固定读取上限，上传原配置保留。普通路径 composer 的原有 1 MB／5 MiB 限额和合法 symlink 合同保持。

此更正替代原条目“暂缓 metadata 64 MiB 上限／预先存在的巨大普通 metadata 仍待处理”的部分；其他批准范围、待测状态、附件二进制资源预算残余和首次失败原件保持。

## 机器判据

本候选新增四项，登记时尚待主协调统一编译执行：

- `upload::tests::upload_json_metadata_budget_has_one_read_write_boundary`：直接校验预算长度，不构造 64 MiB 字符串；read/write 在 limit 与 limit+1 使用同一规则和精确 EFBIG 诊断。
- `upload::tests::upload_sparse_metadata_over_budget_is_rejected_before_read`：对已存在的 metadata 用 `set_len` 扩成 64 MiB+1 稀疏文件，公开 get 返回 EFBIG/read，不解析巨量 JSON。
- `upload::tests::upload_sparse_lease_json_budget_does_not_consume_the_descriptor`：稀疏 lease JSON 经共用读取入口拒绝，fd position 仍为 0，证明未先消费巨大逻辑长度。
- `upload::tests::upload_cleanup_rejects_oversized_json_without_losing_a_live_lease`：同轮跳过巨大 record、回收巨大 lease 元数据、保留真实有效 lease，记录 EFBIG 日志。

继续运行原增长、FIFO、目录 EISDIR 与普通小记录行为回归。定点 rustfmt 和 git diff whitespace 检查已通过；未执行的产品测试和 mutation 不标通过。

## 批准记录

2026-11-15，主协调独审指出原 read_exact_file 仍可被恶意 fstat 长度驱动 OOM，明确要求增加独立 JSON 64 MiB 预算、fd stat 预拒、EFBIG/read、metadata 与 lease 统一合同和稀疏文件测试。实现者同时回报并采用写侧同一预算，避免写出读不回的记录。该安全修复实施授权继承当前用户统一接入计划及基线 bug 默认修规则；不是新增普通附件大小产品偏好。

批准范围及修订已记入 `state/iface-upstream-review.json` 和 `docs/iface-integration/upstream-review.md`。候选验收仍由主协调据真实编译、测试和负向控制决定；本条不授予判分豁免。
