# 独立 DOCX helper：冻结交接

2026-11-13，按根要求停止新增边界专项，交全局 E2E。当前 helper SHA256：`b2a6b27c2751ca5c80084aafdddfe82190ad6ef6b33401f39c5cff95bd5b49de`。源码固定后完成下列验证；文档不把历史候选成绩移到新 hash。仅独占 DOCX 五文件及根授权新增的源替换差异条目；未改 artifact_build、全局资源 manifest、Rust 或冻结标准。

固定参考是 `PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的 `skills/docx/scripts/docx_cli.py` / docxlib；只读理解、未复制源码。实现为标准库 Python ZIP/XML helper，LibreOffice/MuPDF 是外部执行器。PyMuPDF 仅用于独立 oracle，不是产品 helper 的 Python 依赖。

## 当前能力与精确限制

| 能力 | 当前实现 |
|---|---|
| inspect / validate | metadata、paragraph/style/alignment/run格式、headings、嵌套及合并表格、header/footer继承、sections尺寸边距、comments、分part修订计数、fields、图片/外链、package features及coverage、结构错误/警告。不是完整OOXML schema或任意嵌入物语义解释 |
| inspect filters / compare | summary/search/location/max-items及query；各类before/after计数、field multiset增删、metadata/features变化、统一文本diff。完整字段已通过实际固定入口对照 |
| annotate / finalize | main-story段落批注、单run真实del/ins修订及rPr保留；接受/拒绝修订、去comment/引用/OPC部件。跨run、复杂run、move/property/表格复杂修订不伪造成功；与固定基线支持范围及明确拒绝边界对照 |
| sanitize | 清core四字段、自定义属性及其关系、rsid属性，可去comment；不声称清除正文、修订作者和嵌入物中的全部隐私 |
| deliver | 原字节快照再次验证及atomic no-clobber；普通源交付后hash复核；显式源替换及digest recovery。替换源在fsync后precommit复核身份/内容，保留被发现的并发新保存；最后检查到replace不是强CAS |
| render | 一次不可变源快照用于校验、revision和私有0600 source.docx→LO。源码构建MuPDF `draw -F png -c rgb -A8`负责PNG，stext负责文本/quad；保固定matrix-render实际PNG 96DPI元数据。调用者LO registry/config/pack存在时seed到独立profile，否则minimal CJK。字段、hash子目录、自然page-N文件名、文本换行计数、CJK统计及错误details匹配已验用例 |
| CJK | 保固定字符范围、ink<245/3%面积、至少8CJK且paint ratio<0.7的失败阈值；native零高quad用line纵框表达缺paint。合法隐藏CJK不额外拒绝。错误返回同时保留固定基线的诊断PNG；有paint不等于字形语义正确，仍需目视 |
| 资源所有权 | POSIX独立进程组、超时/取消kill并wait、日志上限；Windows renderer owner明确unsupported。21项本地测试包括真实held子进程取消及reap先于返回 |

`--pdf-renderer`明确要求MuPDF mutool；`--mutool`是同一个供给参数的别名，冲突路径拒绝。`PILOTDECK_PDF_RENDERER`是默认，缺省时再用`PILOTDECK_MUTOOL`。不把Poppler CLI当作MuPDF二选一；固定基线原本没有这些原生供给flags。其他技能若需要pdftoppm，应按技能显式传入相应执行器，不能假设一个全局值适用所有Office工具。

## 最终验证身份

- 本机标准库回归 **21/21**；显式固定参考inspect/filter/revision/compare/普通deliver oracle **9格双轮完整JSON相同**。入口：`test_docx_tools.py --baseline-root <固定参考根>`，只在显式oracle中使用参考环境已有python-docx/lxml；默认helper不新增包依赖。
- cb8同机完整实际render oracle **9格×2轮严格通过**：英文、中文、两页表格、72DPI、300DPI、Latin-only fallback、实际无ink CJK错误、合法hidden CJK、隔离生成caller LO profile。两边使用同一输入、系统LibreOffice、同字体配置、同DPI及source-built MuPDF/PyMuPDF 1.27.0。只归一各自输出根，未放宽数值/bbox/像素。
- 每格完整返回或错误code/message/details、实际PDF文本/逐字bbox/字体嵌入hash/像素、最终PNG像素及文件bytes一致；两轮稳定。错误格严格失败并保留一致诊断PNG。**72个engine leader和36个worker均已回收**。
- 同一比较器对有效PNG仅改一个像素的独立副本必须exit1；本地7个独立变异分别破坏inspect/compare、源变化检查、源替换precommit、CJK、hash布局、活输入快照，均被拒绝。未改判分标准。
- 原始render证据本机副本：`_tmp/20261113-docx-full-render-oracle/cb8-evidence/docx-render-parity-20261113-freeze-{first,second}/`。`first/final-receipt.json`、`comparisons.json`、`runs.json`、各格PDF/PNG/rawdict及negative原件齐备。中文及两页三个页面已打开目视检查。
- E0提供oracle环境：source-built CPython 3.12.3、PyMuPDF/MuPDF 1.27.0固定未发布commit ccdefb3；完整source/recipe/module/wheel/smoke身份见`source-runtime-receipt.json`（原件SHA256 `2a847bfcacbbf72529001d86189326d2cf99e19f60c84295f862470044bb9108`）。这是被记录的实际依赖环境，不假称基线精确锁了该版本。

## 已保留的首败及修复

早期13/19测试和macOS英文/中文实际PNG只对应当时hash，原件保留。其后修复了：OPC根namespace被LO拒绝；header关系及style别名/特殊文本；missing content type应warning；render活源与revision错绑（Email独审反例与修后复验均保留）；Poppler最终PNG像素差异（改同源MuPDF）；CJK失败诊断PNG落盘差异；自加source-CJK存在性断言误拒合法hidden文本（删除）；caller profile_seed分支。上述普通差异按固定行为修正，未登记成豁免。第一轮driver猜测29字符失败、实际stext36及初次负控漏带oracle.py的装置失败也保留，不当产品通过证据。

唯一新增窄差异为 `specs/gateway-parity/diffs/D1-DOCX-DELIVERY-RACE-001.md`：固定真实deliver在backup后覆盖并发有效新保存仍success，recovery不含新版本；当前precommit修复按根明确授权登记。它不包含renderer、CJK、字体或普通helper差异。

## 全局 E2E 接续

这次是独立资源及固定入口行为验证，不等于正式Rust loader→模型选择技能→完整创建/编辑/渲染/交付旅程已验，也不等于搬迁发行包已供应引擎/字体。当前完整render对照在Linux同机完成；此前macOS候选做过实际渲染，当前同源MuPDF最终hash未另开新的Mac专项。Windows owner仍未实现。已有读取边界也保留为未闭合：当前总展开上限128MiB，固定参考是总512MiB/单member128MiB；修改带namespace prefix重绑定或AlternateContent仍明确拒绝，未声称覆盖这些输入。按根要求本批不再扩这些边界专项。LibreOffice、MuPDF、适当字体及许可证/可搬迁布局须由统一发行提供。已停止扩边界，后续由全局E2E实际首败驱动精准修复，不以本文件宣称所有Office特性或全部后端完成。
