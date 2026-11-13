---
name: spreadsheets
description: "创建、编辑和审查独立 XLSX 工作簿，使用明确配置的办公引擎重算公式、渲染页面并安全交付。"
---

# 电子表格

先明确权威输入、预期结果、修改边界和验收依据。保留未授权更改的单元格、公式和对象。数字、布尔值和日期应保留实际类型；长编号与前导零标识符用文本。公式文本与已计算缓存是两类证据。

任务脚本、候选、报告和渲染页放在 `PILOTDECK_WORK_DIR`。仅交付用户要求的文件。不要读取真实账号或联网补充未知数据。

## 运行入口与依赖

```sh
SHEET="{{SKILL_ROOT_SHELL}}/scripts/spreadsheet.sh"
bash "$SHEET" check
```

所有操作需要 Python 3。局部 `edit` 依赖环境已安装的 `openpyxl`；使用独立任务脚本创建工作簿时可选择合适的格式库。`recalculate`、`convert-legacy` 和 `render` 使用外部 LibreOffice；`render` 还需要 Poppler `pdftoppm` 与 Pillow。这些工具不是本资源包携带的 portable 引擎。

引擎必须明确给出绝对可执行路径：

```sh
export PILOTDECK_SOFFICE=/absolute/path/to/soffice
export PILOTDECK_PDF_RENDERER=/absolute/path/to/pdftoppm
```

`--soffice` / `--pdf-renderer` 优先于上述变量；兼容旧 `SPREADSHEET_SKILL_SOFFICE` / `SPREADSHEET_SKILL_PDF_RENDERER`。不自动安装或下载。缺失引擎、非零退出、超时、错误结果、空渲染都明确失败，不生成成功占位文件。LibreOffice 使用私有临时 HOME 和 UserInstallation；操作拥有独立进程组，超时会终止该组并回收直接子进程。

## 操作

```sh
bash "$SHEET" validate --input "$CANDIDATE"
bash "$SHEET" recalculate --input "$CANDIDATE" --out "$RECALCULATED" --report "$RECALC_REPORT"
bash "$SHEET" render --input "$RECALCULATED" --out-dir "$REVIEW_DIR"
bash "$SHEET" compare --before "$SOURCE" --after "$RECALCULATED" --report "$COMPARISON"
bash "$SHEET" convert-legacy --input "$LEGACY_XLS" --out "$CONVERTED_XLSX"
bash "$SHEET" deliver --input "$CANDIDATE" --source "$SOURCE" --out "$FINAL"
```

报告是 JSON。`--report` 将同一报告写入新文件。已有输出拒绝覆盖。`compare` 精确报告每个包部件 SHA、增加/删除项与单元格公式、缓存、类型、样式索引差异；`equal:false` 是比较结果，操作仍成功。调用者应按授权修改范围判断，不可把“命令成功”当作“内容相等”。

`validate` 核查 ZIP 路径、重复项、大小限制、XML、内部关系与工作表身份，并报告公式、缺失缓存及错误值。诊断错误单元格不等于损坏包；可读包仍会返回报告。本 helper 专注 XLSX；CSV/TSV 可用标准库 `csv`，宏、加密文件和非 worksheet 工作表不支持。

`recalculate` 不包含 Excel 解释器。它仅在私有副本上调用真实引擎，按相同工作表名、地址和公式身份取得实际结果，把缓存与结果类型合并回原包。原公式字节和其他包部件保留。共享公式会按原锚点平移引用核对，保留原 shared XML；正常错误缓存（例如 `#DIV/0!`）会实际保存并在 validate 中报告。不能可靠合并的数组/数据表公式、外部引用、扩展函数、缺失缓存或 `#NAME?` 结果会失败，整个候选不发布。重算引擎与 Excel 可能存在函数或格式差异；不得承诺完整 Excel 等价。

`render` 导出真实 PDF，并生成每页 144 DPI PNG。必须打开相关页面，检查裁切、数字显示、字体、图表、分页和空白问题，再作视觉质量结论。渲染前若修改了依赖或公式，先重算最终候选。旧预览不能作为新候选证据。

`deliver` 重新验证候选结构，以同目录临时文件加独占原子发布输出，并核对实际输出 SHA。它不代替内容、重算或视觉审查。替换来源只能显式指定 `--source "$SOURCE" --out "$SOURCE" --replace-source`，且必须设置 `PILOTDECK_WORK_DIR`，原文件会保存在内部 recovery 目录。未授权替换、符号链接目标和已有普通输出都拒绝。源替换会在恢复快照前后与最终提交前核对文件身份和字节；检测到并发保存就拒绝覆盖并保留新源。最后一次核对与 POSIX replace 之间仍有非协作写者的竞态窗口，不宣称跨进程 CAS 或全时窗事务保证。

## 局部编辑例程

对于不含包敏感对象的普通工作簿，可以使用独立 JSON 编辑例程：

```sh
bash "$SHEET" edit --input "$SOURCE" --spec "$EDIT_SPEC" --out "$CANDIDATE"
```

规范见 [edit-spec.md](edit-spec.md)。支持单元格值/公式、数字格式、字体、填充、对齐、合并、列宽/行高、冻结、筛选、命名区域及新增 bar/line 图表。已有 drawings/charts/pivot/slicer/externalLinks/嵌入对象/签名的模板拒绝格式库往返，避免静默丢失；这种情况需独立、局部 OOXML 编辑脚本并严格比较。未知规范字段明确拒绝。

编辑会清除公式缓存；保持公式并不证明公式计算正确。完成后独立核对代表性结果、边界和原始来源，修改一个输入验证重算能更新结果，再恢复最终输入。只对实际成功的范围作结论。
