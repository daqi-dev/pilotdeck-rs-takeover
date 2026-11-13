# 独立 DOCX helper 实现与边界

固定参考：PilotDeck cc7c4e6，`skills/docx/scripts/docx_cli.py`及docxlib，源码只读理解、未复制。新实现为`../_tools/docx_tools.py`，技能内`scripts/docx_cli.py`定位该自有helper。没有新增Python包依赖，没有改artifact_build或全局资源manifest。以下为2026-11-13第二批状态，第一批13tests/英文PNG证据保留。

| 固定CLI能力 | 当前独立实现与实际验证 |
|---|---|
| inspect / validate | metadata；paragraph/style/alignment/run格式；headings；嵌套/水平垂直合并表格；继承header/footer；sections尺寸边距；comments；分part修订计数；fields；图片数/外链；macro/active/signature/embedding/chart/diagram/customXML/schema/content-control/textbox/notes/altChunk/math/protection完整字段；coverage limitations；validation errors/warnings/expanded_bytes/xml_parts。固定基线实际Python入口丰富fixture完整JSON一致，不只是字段存在 |
| inspect filters | summary/search/location/max-items及query matches/returned/truncated，headings筛选与tables省略；4种实际基线过滤对照一致 |
| annotate | main story段落批注OPC完整部件；单run非空文本生成真实删除/插入修订，复制rPr并保留旁侧内容；occurrence约束；跨run、复杂run拒绝，基线自身也不承诺跨run自动redline。修订文档inspect实际对照一致 |
| finalize | accept/reject run插入删除，覆盖Word story XML，转换delText，移除trackRevisions；删除comments/markers/relationships/types。move/property/表格复杂修订拒绝，不丢弃后假成功 |
| sanitize | 清core作者等四字段、custom properties及其OPC引用、rsid属性；可删comment；保留正文、revision作者及嵌入物，不宣称全隐私清理 |
| compare | count before/after、coverage、field multiset增删、metadata与package feature变化、统一文本diff，实际基线完整JSON一致；不是单独part字节比较替代 |
| deliver | 原字节atomic no-clobber；候选字节快照再次验证；输出字节核对；源默认保护、显式替换及digest recovery；workdir路径约束；非替换源在发布前快照hash、发布后重新读hash，变化明确source-changed-during-delivery；替换源增加临时写fsync之后、replace之前的身份与内容复核，阻止备份后的并发新保存丢失。测试在真实atomic publication边界写入源变化，必须失败；禁用检查的独立副本被断言拒绝 |
| render / CJK | 显式LibreOffice→PDF、pdftoppm→PNG；CJK另用源码构建MuPDF mutool stext逐字符quad+标准库PNG解码，ink阈值<245、3%面积、页面至少8CJK且paint ratio<0.7时报render-cjk-glyphs-missing。独立HOME/PILOT_HOME/profile；显式font dirs可用，不读用户LO profile；POSIX timeout/cancel killgroup+wait；失败不发布pages |
| self-test | 19项自生成文档测试；技能wrapper同19项。7个独立负控分别破坏inspect count、compare field变化、source-race检查、CJK空白检测、render hash布局、source replacement precommit、恢复live-render-input，全被实际断言拒绝 |

## 固定基线特征证据

`_tmp/20261113-docx-tools/completion/`仅过程证据，不入库。oracle直接调用只读固定资源的Python `inspect_docx/filter_inspection/compare_docx`，设置dont_write_bytecode，不写参考目录。使用自造DOCX，丰富正常inspect、4种filter、修订inspect、compare共7格，双轮完整JSON一致且稳定；另扩充foot/endnote、nonstandard customXML schema、chart/diagram/embedding/ActiveX、content-control/textbox/altChunk/math/保护字段的第8格也与基线完整JSON一致；第9格普通deliver完整返回JSON也一致。可复现入口为现有test_docx_tools.py --baseline-root <固定参考根>；显式oracle依赖参考环境已有的python-docx/lxml，默认测试不增加这些依赖。首比发现header关系namespace/UI样式别名问题，修正后才计通过；扩大features首比也抓到missing content type应warning而非error，已对齐。未把实际入口探测换成自写mock oracle。

第一批现存LibreOffice 26.8.0.3 arm64、pdftoppm 26.06.0，自造DOCX→批注修订→accept/remove comments→真实PNG，打开可见加粗Hello new world。首次两次OPC namespace加载失败原始日志仍在`_tmp/20261113-docx-tools/`；修默认namespace后成功。

第二批`cjk/`真实引擎证明：不供给系统字体时PDF有13个CJK记录但0paint，helper exit3且无成功目录；显式`--font-dir /System/Library/Fonts --font-dir /Library/Fonts`后14CJK/14paint/ratio1，真实PNG打开可读“中文字符实际渲染测试，正确显示。 English 123”。未安装/复制字体。标准库测试另验证白图失败、黑图成功及缺字零高quad不得从分母消失。MuPDF使用源码构建1.27.0，arm64 SHA e4a5126d6c5dadc8dc2554bcded4793cd876bee583d07b429388eee7132dd830，由统一原生组件作者提供。

## 精确边界与发行待项

- inspect/compare本批实现并验证上述完整字段合同，不等于对所有畸形DOCX或所有OOXML schema完成穷尽验证。标准库能保留QName-only namespace声明和XML comments；实际修改的XML若重绑定同一prefix，明确unsupported，避免破坏命名空间；只读inspect允许该输入。AlternateContent分支编辑也未承诺。
- CJK保留基线范围与阈值，但bbox来源是MuPDF sttext，而非未安装的PyMuPDF。缺字zero-height quad使用line纵向范围计算空paint；另外源含至少8CJK而PDF完全无CJK记录会失败。这两条防漏检行为须由根在差异账本确认，不冒称逐像素与fitz完全相同。无需PyMuPDF包。此检查不识别所有错误字形、语义或布局，最终仍需打开页面。
- render普通合同已改为revision[:16]子目录及固定input/revision/render_root/out_dir/pages/images/page_text/cjk_glyph_coverage/text_characters/dpi/libreoffice_profile_seed字段，page-N自然顺序且每非空文本line计换行。合同单测明确使用注入engine；随后第二批当时helper hash 8c80b125cee9c549359b04488d93b6d6996caca8cc297787471f01296017343a又经真实引擎复跑，前后hash不变。final-render-second/results.json记录正常CJK hash目录/profile/字段、14/14paint、8个实际stext文字line（28字符+8换行=36）与缺字体exit3/0paint/无PNG，两分支共6个准确engine leader PID均已reap。PNG与上一轮逐字节相同，已打开检查。现存Python和本任务有限venv/site-packages查找未见fitz；完整固定render入口对拍仍单列待依赖，不能据MuPDF的36自行声称fitz相同。
- 固定baseline replace-source丢数据已实际复现：completion/baseline-race/result.json记录真实deliver_docx在recovery已建、真实os.replace前保存有效新DOCX仍status ok，新源被candidate覆盖且backup仅含旧源。当前precommit修复经根明确授权新增specs/gateway-parity/diffs/D1-DOCX-DELIVERY-RACE-001.md，严格不含CJK/renderer差异；检查到replace之间仍有非协作写者窗口，未声称POSIX强CAS。
- 当前render owned process-group仅POSIX；Windows明确unsupported，未虚称已验证Windows owner。
- 发行仍需统一供给LibreOffice及动态库、Poppler/pdftoppm及依赖、源码构建MuPDF、适当字体与许可/可搬迁布局。现存本机引擎/系统字体成功不表示full包已内置。最终Rust loader、搬迁包、跨平台验证由root/D1统一进行。

## 第三批：render 不可变输入修复

Email只读审查原件`_tmp/20261113-docx-render-review/first0-*/results.json`实证第二批renderer先hash旧源、LO仍读活source，边界新保存后report revision与实际引擎输入不一致。现一次读取immutable bytes，load(input, bytes)、revision、私有0600 work/source.docx、expected_cjk全用同一快照；活source之后保存不影响本次渲染，也不会被还原。

当前helper SHA256为`ea293696c06005d51417fdf8b07966388cc136497cd3d23a31f1bf03cc8e0fee`。增强现有render合同测试，在engine边界把原source改为非ZIP新保存，仍要求实际argv私有文件字节等于旧快照、revision同源、render成功并保留新源。wrapper19/19通过；独立副本恢复活source作为engine argv，断言exit1（snapshot-negative.log）。原先六负控也用修后源码重新执行，均exit1。Email独立读取实际argv[-1]复验passed，证据fixed-first0-b1abwb58/results.json，未覆盖原件。

最小真实复验在`_tmp/20261113-docx-tools/snapshot-render/results.json`：helper前后同hash；两个分支实际LO输入都为私有source.docx且input SHA==revision；正常CJK14/14、36个提取字符统计、hash子目录/profile保持，缺glyph仍exit3且无PNG；六个准确engine PID均reaped。PNG仍SHA80670557b606b3c56c4f1cffc960c8bdc766b3e8a2d60d0cd6090db3d4a5f353，与已打开的正常图逐字节一致。完整fixed render对拍继续等Native源码构建fitz依赖，不把这次修复实证扩大为完整parity。首次新增负控启动遇ENOSPC已报根，未运行；盘余恢复后才补执行成功，未删他人文件。
