---
id: IFACE-UPLOAD-SAFE-HANDLE-001
---

## 类别

基线 bug／安全修复。FIFO 等待无写入者会挂起，路径 stat 后增长会绕过既有 inline 大小限制；上传工件被替换为符号链接、FIFO 或设备后不得继续跟随或读取。仅修这些明确的异常路径，不增加合法附件目录限制、默认大小上限或一般行为偏好。

## 基线依据

固定 TS `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，只读原源码：

- `src/gateway/dialog/UploadStore.ts:369–391` 的 `readAt` 在 realpath 后调用 `readFile(canonicalPath, "utf8")`；`:433` 的 lease 元数据读取同样使用 `readFile`。
- `src/context/attachments/AttachmentResolver.ts:36–37` 已有文本 1,000,000 字节／图片 5,242,880 字节限制，`:129–183` 先 `stat` 再 `readText`。`AttachmentPort.ts:25–31` 转发给 Node 文件读取，`NodeFsPort.ts:32–38` 最终使用 `nodeReadFile`。
- 本轮实际在隔离 HOME/PILOT_HOME 中调用原 `AttachmentResolver`：合法普通 symlink 成功；stat 采到 5 字节后文件长到 1,004,096 字节，原 resolver 仍内联全量且无超限诊断；原生 FIFO 调用 1.5 秒后仍未返回，外层为 15 秒 own-PID watchdog SIGKILL（不是自然退出）。准确 PID、退出码和输出见 `_tmp/20261115-iface-upstream-baseline/run-k_7ivs3a/receipt.json`。
- 本轮实际调用原 `UploadStore.create/get`：正常记录为 `created` 且可读；将相同 metadata leaf 替换为 FIFO 后，1 秒发出 TIMEOUT，5 秒 own-PID watchdog SIGKILL。原件 `_tmp/20261115-iface-upstream-baseline/upload-jzcs7kzb/receipt.json`。首次探针漏填 relativePath，`UPLOAD_MANIFEST_INVALID` 属于装置 INVALID，保留在 `upload-7wjdu4g7/receipt.json`；第二次补足 manifest，未改产品源码。

以上收据包含源码与探针 SHA、Node v22.22.0、隔离目录和进程退出身份。它们只证明上述场景，不代替全入口对拍；所有这些探针进程均已回收。

## Rust 行为

- `src/pilotdeck-gateway/src/upload/mod.rs`：Unix 上传工件打开使用 `O_NOFOLLOW | O_NONBLOCK`，同一描述符检查类型和既有身份。metadata 和 lease 元数据按已采 fd 长度读取，短读与尾部增长均拒绝；附件 digest 按已核对长度读取，避免追逐持续追加的数据。FIFO／设备不进入读取。metadata 的攻击窗口 symlink 在 open 阶段返回 Node 形 `ELOOP`，比旧 Rust 的后置 containment 拒绝更早。
- `src/pilotdeck-gateway/src/native_process/services.rs`：普通路径附件保留合法 symlink 跟随，仅增加 `O_NONBLOCK`；同 fd 判定 FIFO／设备时以现有 `gateway_submit_failed` 和 `EINVAL: invalid argument, read` 形拒绝。普通文本／图片最多读取原有限额加 1 字节，超限走原有 skip 诊断。并发增长的诊断中 cap+1 表示实际已读取的超限前缀，而非完整文件快照。
- 原合法文件、项目外普通路径、合法 symlink、空文件、原有 cap 两侧、目录 `EISDIR` 和既有注册／权限合同必须继续通过。普通路径没有重新加入 main 旧版的 lease-root 限制。

## 范围

仅上述上传 store 和普通路径附件 inline 读取的安全边界，目标平台 macOS/Linux Unix。非 Unix 不声称具有 `O_NONBLOCK/O_NOFOLLOW` 防护。

不接受 upstream main 新增的 metadata 64 MiB／attachment 1 GiB 固定读取上限，不改变可配置上传限额和历史记录格式；不引入 main 的 list 不可读记录跳过政策。预先构造的超大普通 metadata，以及声明长度匹配的巨大附件，仍需后续统一资源预算处理；本条不声称关闭该残余。PDF stat-only、音频跳过和已有内容直供路径不扩展到新文件读取。

本条不授权更改冻结判分文件、不整体忽略错误码/消息差异，也不把接口可用或编译通过当作行为验收通过。

## 机器判据

Rust 本候选待运行（当前已编码并通过定点格式与 diff whitespace 检查，尚未编译／运行）：

- `upload::tests::upload_lease_cleanup_does_not_wait_on_a_fifo_lease_entry`。
- `upload::tests::upload_verify_attachment_of_a_fifo_leaf_is_rejected_within_the_watchdog`、`upload_verify_attachment_never_follows_a_symlink_leaf`。
- `upload::tests::upload_read_at_metadata_swapped_for_a_fifo_is_rejected_within_the_watchdog`、`upload_read_at_metadata_swapped_for_a_dev_zero_symlink_is_rejected_at_open`、`upload_read_at_record_grown_between_stat_and_read_is_rejected`。
- `upload::tests::upload_read_at_metadata_hardlinked_through_a_swapped_id_dir_is_rejected` 及原 rename/inode/lease 行为回归。
- `native_process::services_tests::iface_attachment_fifo_is_rejected_without_waiting_for_a_writer`、`iface_attachment_device_symlink_is_rejected_before_read`、`iface_attachment_regular_symlink_remains_readable`、`iface_attachment_growth_after_descriptor_stat_stays_bounded`。
- 原 `path_attachments_are_read_wherever_they_are`、`read_failure_fails_the_turn`、`exact_limit_image_and_text_are_inlined` 和 `limit_plus_one_attachments_skip_to_diagnostics` 作为正常行为对照。

编译与测试由主协调单 heavy 队列运行 gateway `upload::` 和 `native_process::services_tests::`，随后受影响 Clippy 与阶段检查。负向控制应分别移除非阻塞打开、fd 类型限制和读取上界，并确认对应 FIFO/设备/增长测试捕获错误；本条登记时尚未执行这些 mutation，不能写成已判杀。

## 批准记录

2026-11-15，主协调依据本会话用户“PLEASE IMPLEMENT THIS PLAN”授权及既有基线 bug 默认修规则，明确批准先接收上传句柄安全子集，随后批准普通 path 附件的窄适配：保持正常 symlink、保持现有限额、必须检查实际 fd，不回退到 main 旧 lease-root 行为。实施审阅记录保存在 `state/iface-upstream-review.json` 的 `approval` 字段以及 `docs/iface-integration/upstream-review.md`。

本批准是安全修复范围的实施批准，不是测试通过或交付批准；本候选编译、行为回归和负向控制仍待完成。原上游批准和原上游通过记录不自动适用于本候选。
