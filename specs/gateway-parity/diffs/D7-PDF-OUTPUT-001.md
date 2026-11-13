---
id: D7-PDF-OUTPUT-001
---

## 类别

bug 修复：PDF 技能 build 的输出竞态可能覆盖并发保存的新文件、丢失用户字节；失败 builder 会把部分坏 PDF 留在选定最终路径。附带严格检查 render 声称的 PNG 实际格式，避免 JPEG 字节冒充 PNG。不是普通行为改进，不新增判分豁免。

## 基线依据

固定 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，`skills/pdf/scripts/pdf_cli.py:371-393` 的实际 `command_build` 将最终 `--out` 直接交给任务 builder，再 audit。实际运行该固定 Python 入口，在输出路径解析后、builder 执行前注入并发用户保存，正常覆盖型 builder 覆盖该版本，入口仍返回 0；原始证据 `_tmp/20261113-pdf-cross-review/baseline/results.json`。基线技能没有独立 deliver 子命令，本更正不发明该命令。

原独立资源候选 SHA256 `17d03ee23227f541f3936eaa365c4da6b46b6220269e2878282fbaf163d7b696` 也复现并发覆盖、失败部分输出及 JPEG 命名为 `.png` 后返回成功，原证据 `_tmp/20261113-pdf-cross-review/results.json` 保留。

## Rust 行为

Rust 后端携带的独立 Python 资源 `packaging/backend-resources/skills/_tools/pdf_tools.py` 保持 build 和 render 参数、正常成功 JSON 及原有最终输出位置。build 仅把私有候选路径交给任务 builder，读取不可变字节快照、对同一快照 audit，再写入最终目录临时文件并 fsync，以独占硬链接原子发布完整字节。最终路径已存在或在构建期间被其他写者创建时拒绝，不覆盖它；builder 失败及 audit 失败只影响私有临时文件。

render 在完整解码图片前要求 Pillow 检出的格式为 PNG，不能只凭扩展名或解码成功。

## 范围

仅资源 PDF build 的并发输出保护、失败清理和 render 实际 PNG 格式。不是 OS sandbox，不约束任务 builder 自行访问的其他路径；不宣称跨非协作进程的全时窗事务。没有改变 merge、split、rotate、forms、ReportLab 分页与字体策略，没有加入覆盖授权或重新定义普通成功报告。

## 机器判据

`packaging/backend-resources/skills/pdf/scripts/test_pdf_tools.py::Workflow.test_atomic_build_and_actual_png_format_guards`：在原 fresh 检查后注入新最终文件，要求拒绝且保留该文件的全部字节；失败 builder 写坏候选再退出，要求无最终文件；JPEG 字节命名 PNG 必须被拒绝。

修后结构反例 `_tmp/20261113-pdf-cross-review/fixed-guards.log` 通过。最小真实 ReportLab 4.2.5 scaffold→build→audit→源构建 MuPDF render 流程 `_tmp/20261113-pdf-cross-review/fixed-real/results.json` 通过，实际一页 PNG 已打开检查。原先完整创建、分页和四页渲染证据继续保留，不用新样本替换历史失败。

## 批准记录

本轮用户 AGENTS 明确允许修复固定基线的崩溃、挂起、丢数据和安全 bug，要求登记已接受差异。主协调者于 2026-11-13 在本恢复会话明确批准这两个已实证缺口的窄修：严格 PNG；build 私有候选、audit、同目录 stage、独占原子发布，禁止覆盖并发新 final，失败不得留下坏 final；同时要求固定真实基线竞态证据并新增本条。本条不修改冻结 accepted-diffs.md、FROZEN.md 或任何比较器。
