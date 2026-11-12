# S1 会话归档原子恢复取证

固定基线 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`、Node `v22.22.0`。`metadata.json` 固定正式入口、恢复回调、writer、persistence 的源码和 dist 共 8 个 SHA-256。参考目录只读；驱动不安装依赖，不调用 Cargo。

这是正式 `pilotdeck server` 进程上的 WS `restore_session_transcript` / `export_session_transcript` RPC 取证。存储失败是明确注入的：TS 通过 `--import ts-append-fault.mjs`，仅在本次隔离 PILOT_HOME 下 `.restore.tmp` 文件的真实 `appendFile`、`accepted_input` 条目写前注入 EIO 或屏障；没有替换 Gateway、writer、归档回调或参考源码。Rust 使用已有 `PILOTDECK_NATIVE_STORAGE_STIMULUS` 的同一 writer-site key，不增加新的产品故障开关。两侧都在标题元数据已经落盘后、消息写入前触发。

在 checkout 根目录运行，新输出目录必须不存在：

```sh
mkdir -p _tmp/20261113-s1-session-restore
node src/pilotdeck-gateway/tests/fixtures/s1-session-restore/capture.mjs ts _tmp/20261113-s1-session-restore/ts-run-1
node src/pilotdeck-gateway/tests/fixtures/s1-session-restore/capture.mjs ts _tmp/20261113-s1-session-restore/ts-run-2
python3 src/pilotdeck-gateway/tests/fixtures/s1-session-restore/compare.py _tmp/20261113-s1-session-restore/ts-run-1/observed.json _tmp/20261113-s1-session-restore/ts-run-2/observed.json
node src/pilotdeck-gateway/tests/fixtures/s1-session-restore/capture.mjs rust _tmp/20261113-s1-session-restore/rust-run-1 /absolute/fixed/pilotdeck
```

`capture.mjs rust OUTPUT BINARY unsafe` 专用于记录旧缺陷：要求第二段失败和中断后都有最终半文件、同 key 重试报冲突。它不能作为修复通过结果。默认 `safe` 期望恰好相反，旧候选必须失败。

所有服务用随机本地端口、独立 HOME/PILOT_HOME/项目目录、白名单环境；模型 provider 仅是本地 HTTP mock，整个场景必须 0 请求。驱动持有具体 ChildProcess，只对该 PID 发 SIGTERM / SIGKILL。健康 HTTP 关闭 keepalive、WS 正常关闭后再停服务；所有 RPC/启动/清理等待有界。`observed.json` 保存命令、环境、PID、完整 stdout/stderr/RPC、文件状态、驱动和二进制哈希；失败保存 `failure.json`，既有输出不会被覆盖。

七段覆盖（其中占用目标一段有五格）：

1. 第二段 append EIO：RPC 失败，最终路径不存在，普通异常下临时文件已清理。
2. 同 sessionKey 重试：成功，包含完整标题与消息。
3. 正常有标题/无标题恢复、已有目标拒绝且字节不变；JSONL 顶层及 metadata 键序、sequence、parentEntryId/turnId 关联保持，正式 export 返回原 archive。
4. 五格正式目标边界：已有目录、指向普通文件的活 symlink、悬 symlink、指向目录的活 symlink、完整 stage 提交前外部创建目标；记录原目标及 referent 字节/种类。
5. 结束进程、重启同目录，再正式 export 返回原 archive。
6. 第二段 append 屏障：见证标题已经落盘；此时 SIGKILL 模拟进程级中断。最终路径必须仍不存在。
7. 重启后原会话仍不存在，同 key 再恢复成功。

进程 SIGKILL 无法执行 RAII / TS catch 清理，所以可能遗留独立临时文件。它不带最终 `.jsonl` 名字，不被提升为会话；下一次恢复用新的唯一 stage。实现只清理自己拥有的 stage，不扫描删除可能属于其他并发写入者的文件。正常失败和请求 future 取消的清理由 Rust 十个 `session_archive_s1_tests.rs` 测试补充；正式入口驱动没有把进程中断冒称为请求 future 取消。

Rust 修复采用同目录临时文件与 `hard_link` 原子发布，保留既有 writer 序列化。它比 TS `rename` 的窄差异是：在写完整 stage 后有外部参与者抢先创建目标时，明确冲突并保留其字节；绝不退回覆盖 rename。普通文件、目录和活/悬 symlink 均视为占用。硬链接其他失败保留原始错误。`_tmp/20261113-s1-session-restore/filesystem-atomic-proof.json` 记录本平台 link 拒绝已存在目标而 rename 会覆盖的实测。

本轮修前 CLI 固定副本 SHA-256 为 `7ba4eabc0738974f7c80a2d37062ff56f90c740e66eab69d942472552fa38f59`，复制前后核对相等。现有 `_build` 未找到绑定该 hash 的完整构建回执，因此它只标记为固定的修前二进制证据，不冒称当前 WIP/HEAD 构建。修后 Rust 成绩由根协调者统一构建并运行后登记；未编译的单测不能算通过。

正式 TS 两轮 `ts-verified-1` / `ts-verified-2` 已各通过七段、三次进程、0 provider 请求；`self-compare.json` 记录完整业务操作的自比，只归一化本次私有路径、保持关联的 UUID 和已校验 ISO 时间戳。成功 JSONL 键序也参与比较。当前驱动 SHA-256 为 `d1dc8a7b46ce890158f67dba2c551f118d9942d47655310750b754a7b00db3db`。旧修前二进制使用同一 safe 驱动的 `rust-before-negative-final` 在真实半文件上被拒绝；这是运行真实旧产品的负控，未靠篡改观察 JSON。

实际 TS 五格结果：目录返回 `EISDIR`；指向普通文件的活 symlink 返回 `SESSION_RESTORE_CONFLICT` 并保留原链接与原文件；悬 symlink 和指向目录的活 symlink 都被 rename 替换为完整 archive，referent 不受影响；屏障后外部创建的目标被 TS rename 覆盖。Rust 对所有已占用目标明确冲突，其中活文件 symlink 一格与 TS 相同，其余四格属于主授权的窄安全差异，不宣称 baseline parity。`filesystem-atomic-proof.json` 只是机制补证，正式差异以这五格 RPC 证据为准。

错误处理单独验收：原生存储错误保留真实 errno/code；已有 Stage 提供实际 syscall/path。无 Stage 的 StorageStimulus EIO 仅保留原始 errno 和原错误文字，不复制 TS preload 人工消息、不猜 syscall/path。普通 ENOTDIR/EACCES 与实际 link ENOENT 有真实系统调用测试；非 IO LedgerError 不伪装 errno。既有 Rust-only LedgerLock.open 明确擦除原 errno 的边界保持不变，新接口不会从其文字反推 code。正式权限测试须设 `PDRS_REQUIRE_PERMISSION_FAULTS=1`，特权宿主无法触发 EACCES 时不能把 skip 计为该格已通过。
