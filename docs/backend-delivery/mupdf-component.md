# MuPDF 原生组件准备

2026-11-13：本机和 cb8 的 `command -v mutool` 均未找到工具；固定 TS 基线安装的 `mupdf/package.json` 为 `1.27.0`。当前不把 PDF 图像能力写成已交付。

源代码固定到 [ArtifexSoftware/mupdf 1.27.0](https://github.com/ArtifexSoftware/mupdf/releases/tag/1.27.0)，完整提交 `d3b7556577b790e9761868c314ad6fd9b6dd86a9`。须按该提交初始化所有 submodule，保存各子模块 SHA，不使用当前主分支。官方 [1.27.0 安装说明](https://mupdf.readthedocs.io/en/1.27.0/guide/install.html) 提供 `make tools`，仅构建命令行工具时无需 GUI/X11 依赖。

cb8 来源目录：`/home/clawbox-08/ws/pilotdeck-rs-cc2c/tools/src/mupdf-1.27.0`；抓取日志 `logs/fetch-mupdf.log`；固定源码和全部子模块已获取完成。构建由 `scripts/backend-validation/build-mupdf.sh` 执行，使用任务构建锁，以外置 `components/mupdf/Makefile` overlay 复用上游静态库和链接参数，构建 mutool 与 `pilotdeck-pdf`。输出到任务专属 `tools/mupdf/bin/{mutool,pilotdeck-pdf}`，不覆盖全局安装。产品候选 `14351f4e1d53b2262015cfea484d811342204ba3` 的组件已源码构建、安装及源码归档通过，日志 `logs/14351f4e-mupdf-build`。

[官方许可说明](https://mupdf.readthedocs.io/en/1.27.0/license.html) 声明 GNU AGPL，并另有商业许可选项；固定源码 README 明确为 AGPL-3.0-or-later。本项目按开源源码分发路径准备组件；发行材料须包含 MuPDF 的 `COPYING`、所编译第三方库的各自许可、完整源码/子模块身份和可复现构建记录。构建脚本复制顶层 `COPYING`，归档固定 MuPDF 源码及全部 thirdparty 源（保留其各自许可，排除 Git 元数据与生成的 build 目录），另带 helper 源、项目 LICENSE、完整提交/子模块清单与二进制 SHA-256；发行工作包必须保留这些归档，不只拷贝二进制。

预定检查：`mutool -v`，静态/动态依赖清单，最小 PDF 文本提取、页数、逐页 JPEG（质量80、invert_cmyk=0）输出、损坏 PDF 与取消/输出上限，再通过正式 Rust 工具入口与固定 TS 基线对拍。Rust 宿主由 T1 负责 `PILOTDECK_PDF_TOOL` / 发行组件路径解析、超时、取消、输出上限和缺组件错误。本文件不代表这些接线与行为检查已经通过。

只读核对：MuPDF 1.27.0 的 mutool draw 没有 JPEG 输出格式，helper 直接调用 `fz_write_pixmap_as_jpeg`，与 JS `asJPEG` 的 buffer 包装使用同一个编码器。页图使用 100/72 比例、DeviceRGB、alpha=0。API 相同没有被作为通过依据，原始 JPEG 字节另行实际对拍。

实际底层验收 `check-pdf.mjs` 已通过 6/6：正常 PDF 页数、全文及 JPEG 原始字节与固定 TS 的 MuPDF 1.27.0 完全相同，越界页、巨幅页像素上限及损坏 PDF 均以 exit 1 返回，未发生信号退出或 spawn 错误。完整日志 `logs/14351f4e-pdf-component-check`；manifest 保存 helper 来源提交、MuPDF 来源提交、基线提交与 WASM 哈希、fixture 哈希。helper SHA-256 为 `bf370bfe6463d644b29f466b01ad3ac8bbac194fdb53bed860cfeb6e88ee69cb`。正式 HTTP/工具入口与最小图像预算仍在验收队列，不能用这六项替代。

Linux ARM64 构建产物的动态依赖只有 libc、libm 与系统 ELF loader；符号版本最高需要 GLIBC_2.38，记录在 `logs/14351f4e-mupdf-build/{runtime-libraries.txt,elf-versions.txt}`。此产物不是旧 glibc Linux 或其他架构的发行验证。
