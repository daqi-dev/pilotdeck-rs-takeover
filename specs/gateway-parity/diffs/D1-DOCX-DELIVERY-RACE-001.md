---
id: D1-DOCX-DELIVERY-RACE-001
---

## 类别

已实证丢数据 bug 修复。DOCX `deliver --replace-source` 建立旧源恢复副本后、最终发布前发生有效的新保存，固定基线仍覆盖新保存并报告成功，恢复副本不含被覆盖的新版本。只修复已发现的源替换窗口，不豁免正常 DOCX helper 合同。

## 基线依据

固定 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的 `skills/docx/scripts/docx_cli.py:109-116` 将真实 deliver 参数转交 `docxlib.delivery.deliver_docx`。`skills/docx/scripts/docxlib/delivery.py:101-109` 先 `_backup_source`、取源 hash、写临时文件并验证，然后直接 `os.replace`；`:110-111` 的源 hash 复核只适用于 `not replacing_source`，源替换没有提交前复核。

2026-11-13 macOS 实际调用固定资源内的 `deliver_docx`，使用自造有效 DOCX、隔离 HOME/PILOT_HOME/PILOTDECK_WORK_DIR、禁止参考目录 bytecode 写入；在真实 `os.replace` 边界注入另一份有效自造 DOCX 的并发保存，确认 recovery 已存在后才注入。原件 `_tmp/20261113-docx-tools/completion/baseline-delivery-race.py` 与 `baseline-race/result.json`：返回 `status: ok`，`newSourceLost: true`、`publishedCandidate: true`、`backupHasOldSource: true`、`backupHasNewSource: false`，记录 old/new/actual/backup 四份 SHA-256。未使用账号、用户文档或模拟的成功输出代替真实基线函数。

## Rust 行为

本项实现位于 Rust 后端发行资源的独立 Python helper：`packaging/backend-resources/skills/_tools/docx_tools.py:927-956` 先读取源身份（device/inode/size/mtime_ns/ctime_ns）和完整字节，读前后身份不一致即失败；恢复副本根据这一快照写入并逐字节验证。`atomic_bytes:156-164` 完成临时写入和 fsync 后、实际替换前调用 precommit；源替换在该回调再次复核身份和字节，检测并发保存时返回 `source-changed-during-delivery`，保留新源及旧恢复副本。普通非替换源交付仍在发布后按基线复核源 hash，不修改正常成功结果。

## 范围

仅 `deliver` 同时给定 `--source`、`--replace-source` 且最终目标确为该 source 时，对本次源替换提交前可观察到的并发保存增加保护。正常候选字节、普通交付返回字段、恢复副本命名/hash、显式源替换与 overwrite 选择保持各自合同。不是任意外部写者的事务/CAS：最终检查与 POSIX replace 间仍有不可消除的非协作写者窗口，未声称该窗口绝对关闭。

本项不涵盖 CJK/fitz bbox、renderer 布局或文本计数、字体/引擎供给、Windows 进程树或其他 helper 普通行为差异，也不允许因条目存在就忽略这些合同。

## 机器判据

`packaging/backend-resources/skills/docx/scripts/test_docx_tools.py::DocxToolsTests.test_source_replacement_rejects_concurrent_save_after_backup` 在恢复副本实际建成后、真实临时文件写入/最终发布路径中注入新保存；必须拒绝并保持新源原字节及旧恢复副本原字节。`test_source_replacement_requires_explicit_mode_and_keeps_backup` 验证正常显式替换及恢复副本，`test_delivery_detects_source_change_during_atomic_publication` 保留普通交付的基线源变化错误路径。

独立 helper 副本将 `precommit=verify_source_before_replacement` 改为 `precommit=None` 后，前述并发保存测试必须 exit 1；原始结果 `_tmp/20261113-docx-tools/completion/source-precommit.negative.log` 与 `negative-results.json`。实际 wrapper 回归 `python3 packaging/backend-resources/skills/docx/scripts/docx_cli.py self-test`；显式固定基线 oracle `python3 packaging/backend-resources/skills/docx/scripts/test_docx_tools.py --baseline-root /Users/da/ws/pilotdeck-rs/resources/PilotDeck` 包含正常 deliver 完整 JSON 对照。原件未被登记覆盖；新增条目不改变判分脚本、阈值、分母或失败判断。

## 批准记录

2026-11-13 用户 `docs/plan/backend-completion.md` 明确授权基线崩溃、挂起、丢数据和安全问题修复，并要求正常协议保持兼容。本轮主协调者在收到真实固定基线数据丢失原件后明确授权：“将真实Docx source-race已证丢数据写新增窄D1-DOCX-DELIVERY-RACE-001差异条目并引用原件，root授权登记；不要把尚未实证fitz零高bbox归进该条。”本条按该既有用户授权与本轮明确登记指令记录该窄 bug 修复；不是另一次人类逐条签字，不扩大为普通 helper 行为豁免，也不替代产品测试或独立审查。
