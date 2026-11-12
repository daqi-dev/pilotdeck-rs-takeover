# S1 恢复候选接手审查

2026-11-13；只读审查，C1 HEAD `bcad7cb1435103e31c9ec71877c7064f3cffc619` 加未提交修改；根核对时 HEAD `e8618392e7e4eb24c227eedd28e14c84237ca8a4`。未运行 Cargo、产品、TS 或测试，未修改产品源码。既有回执不是当前候选的通过证明。

## 结论与风险

S1 可作为独立窄批准备集成，静态审查未发现必须先改源码的阻断问题，但当前**待验证**，不应标通过或交付。restore 先检查目标并取得 canonical fence 和既有 writer lease；阻塞 writer 持有 reservation、fence、lease，临时文件完整写入和 sync 后 hard-link 发布，失败清理临时名；取消与最终发布竞争仍可能产生完整结果，不能宣称取消保证无提交。目录、软链和外部竞争无覆盖是安全差异，见 `specs/gateway-parity/diffs/S1-RESTORE-NO-REPLACE-001.md`。跨平台文件系统 hard-link 支持、崩溃后目录持久性以及正式入口实测尚未由本审查验证。

共享 `runtime/session/factory.rs` 修改仅抽出 `try_writer_lease_storage`，旧 API 继续转换为原 RuntimeError，沿用相同 canonical mutex 和 LedgerLock；H1 ownership 主要在 `handle.rs`、`completion.rs`、`submit.rs`、交互注册等文件，未发现直接重叠。根对应 factory 与 C1 HEAD 字节相同，仍应由根唯一写入人做集成，不能将 H1 整批带入。

固定 TS `src/cli/createLocalGateway.ts:1315-1369` 先写临时 transcript 再 rename；`:1643-1647` 的 export 顺序为 schemaVersion、format、messages、title。两轮已保存的实际 RPC 首次/重启帧也均为该顺序（`_tmp/20261113-s1-session-restore/export-key-order-proof.json`）；输入 title 在 messages 前。当前 `expected_export()` 保留完整有序断言，修正 oracle 有独立依据，不是修改产品迎合测试。比较器对普通对象键序不敏感且 UUID/path 归一宽，不能外推一般严格 parity。

首轮日志 `_build/20261113-044959-s1-first/output.log` 的七项，包括权限项，均在最终完整 archive 比较失败；权限项已越过真实 PermissionDenied probe、EACCES/code/lstat/path 断言和权限修复重试，最后因输入顺序误作 export 期望失败。identity 的 `permissionFaultsRequired=true`；这证明当时执行到这些断言，不代表整项通过，修正后仍须强制权限模式重跑。当前测试源码修正尚未编译，首轮 3 passed / 7 failed 保留。

## 精确集成范围

- 五个既有文件的 S1 diff：`native_process/session_archive.rs`、`runtime/session/factory.rs`、`storage/mod.rs`、`storage/transcript.rs`、`storage/transcript_adapter.rs`；根上述五个文件与 C1 HEAD 原始版本逐一一致，可应用这些 diff，应用前重新核对以避免并发更改。
- 新增 `native_process/session_archive_s1_tests.rs`；以及 `tests/fixtures/s1-session-restore/` 下 `capture.mjs`、`ts-append-fault.mjs`、`compare.py`、`metadata.json`、`README.md`。
- 新增上述 S1 差异记录、`docs/backend-delivery/S1-C1.md`、`S1-C1-review.md` 及本审查。不要携带 C1 的 H1、O1、SDK 修改或整份交接文件；不需要 Cargo 依赖或 assembly 变更。
- 集成后冻结完整源码身份，先运行强制权限的十项和已有 restore 回归；再以有源码/二进制身份的正式 CLI 运行 safe 驱动，覆盖中段 EIO、重试、普通成功、占用、SIGKILL/重启及 export。保留首次失败，核对真实系统调用错误来源。里程碑四项全量检查仍须在最终集成 SHA 执行。

范围仅会话归档恢复，不代表 S1 整包或后端完成。参考资料与真实用户 home 未改动。
