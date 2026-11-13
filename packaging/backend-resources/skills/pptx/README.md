# Native PPTX resource helper

独立实现，不复制固定参考技能/helper；本资源新增Python标准库ZIP/XML结构处理、安全交付和明确外部转换接口，默认核心不需要Node。五个命令名与固定基线的技能入口一致：validate、render、compare、deliver、convert-legacy。输出为机器可读JSON或明确非零错误；本实现不声称逐字节复刻旧Node helper的完整报告schema。

## 固定基线依据

只读`OpenBMB/PilotDeck@cc7c4e6/skills/pptx/scripts/pptx_cli.mjs:15-80`声明五个命令和CLI flags。`lib/paths.mjs`规定内部工作区/最终路径及源区分；`lib/delivery.mjs`规定显式源替换与恢复副本；`lib/convert.mjs`检测OLE并要求LO实际生成可验证PPTX；`lib/render.mjs`使用独立profile、LO→PDF和显式rasterizer；`lib/compare.mjs`实际比较OOXML parts/text/notes，不是像素差。源码仅用于理解合同，产品处理逻辑独立编写。

## 引擎合同

统一`PILOTDECK_SOFFICE`、`PILOTDECK_PDF_RENDERER`（兼容旧PPTX专有env），命令上的`--soffice`/`--pdf-renderer`优先。仅接受已存在、可执行的绝对路径，不探测PATH或自动安装。外部引擎按参数数组运行，不经过shell；LO始终得到独立`-env:UserInstallation=file://...`。用户中断/显式timeout结束当前owned进程组并等待，不管理其他Office进程。没有声称约束自行脱离进程组的恶意引擎。

标准库足以运行validate/compare/deliver/内部replace_text。render还需要Pillow验证真实PNG，以及LO和pdftoppm或mutool；没有引擎时不返回空slides成功。相同文件名旧render目录拒绝覆盖，使用新的review目录。转换和渲染包含引擎兼容性提示，结构检查通过不代替视觉QA。

## 样本与验证

`tests/test_presentation_tools.py`用已安装python-pptx自行生成可编辑两页和speaker notes，不使用参考材料的样本或预编译产物。14项测试覆盖结构/notes、dup ZIP、traversal、DTD、丢失relationship、局部文本编辑只改实际slide部件、原子交付/不覆盖/源保全/显式替换恢复、symlink工作区逃逸、坏ZIP、缺引擎、转换不产物/假引擎实际有效OOXML、渲染页数不符/假PNG/真实PNG与独立profile参数。

```sh
python3 -B -m unittest discover -s packaging/backend-resources/skills/pptx/tests -v
```

测试中fake office/PDF/PNG仅证明调用、错误检测及资源合同，不是Office渲染验收。首次追加两个mock用例时，mock脚本bytes字面量转义错误导致SyntaxError；首次14项日志保存在`_tmp/20261113-resource-pptx/pure-first0.log`。修正测试脚本编码后14/14通过，记录`pure-fixed-mock.log`。更早12项通过记录`pure-before-png.log`保留；不以重跑抹去失败。

真实LO/render/convert由根单独授权的light槽执行，使用自行生成样本、独立HOME/PILOT_HOME与profile；运行命令、真实PID/PGID、引擎版本/二进制SHA及图像验证记录到`_tmp/20261113-resource-pptx/real-first0/manifest.json`。真实first0退出0、manifest status=passed：LibreOffice 26.8.0.3与pdftoppm 26.06.0完成源/候选各两页960×720 PNG，人工目检标题与正文完整无裁切；未改第二页PNG SHA相同。自行生成真实OLE binary PPT后转换回两页PPTX并保留源hash。validate、局部编辑、结构compare、原子deliver、未授权源覆盖负控和显式替换恢复均通过，8个owned引擎进程均exit 0。原始文件、候选、notes及recovery均有独立检查；这验证当前样本，不声明所有Office高级特性的视觉兼容。

交付还需根统一打包资源账本、真实skill loader和搬迁后调用验证；本作者不改globalmanifest、已有artifact_build.py或Rust源码。

## P1 独立审查修复

源替换在创建完整恢复副本后、最终原子发布前重核原文件的dev/inode/size/mtime/ctime与字节；已变化则拒绝提交并保留恢复副本。该检查覆盖已发生的并发保存，不构成对任意外部写者的事务或强CAS：POSIX check与replace之间仍有不可消除的窗口。普通symlink输出沿既有canonicalize合同处理。超时/中断无论leader是否已退出都终止自有PGID，后续pipe清理有界，直接子进程等待回收。

新增原精确反例测试：source在backup后/publish前保存，必须拒绝且新保存与旧恢复副本均保留；leader退出但孙进程持pipe，0.1s timeout必须在1.5s内收束。首次测试的临时目录在macOS canonicalize前后不同（/var与/private/var），注入条件未触发，日志p1-fix-first0.log保留；修正测试路径身份后结果另存p1-fix-second.log。
