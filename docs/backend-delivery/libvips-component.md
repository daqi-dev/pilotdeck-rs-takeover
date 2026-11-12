# libvips / Sharp 源码组件准备

2026-11-13，Linux ARM64 源码组件已构建，真实图像行为对比进行中，不能据此声明图像工具交付。

固定 TS 基线 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的实际 `sharp.versions` 显示 Sharp 0.34.5、libvips 8.17.3；`@img/sharp-libvips-darwin-arm64` 包版本 1.2.4。对应官方 [sharp-libvips v1.2.4](https://github.com/lovell/sharp-libvips/tree/v1.2.4) 固定提交 `20b5e899954907a3039d6e3d4c200aaa0ec52c4c`，其 `versions.properties` 与本机实际清单逐项吻合。libvips 官方 v8.17.3 对应提交 `0c9151a4f416d2f8ae20a755db218f6637050eec`。只取源码，不下载其发行的预编译包。

完整固定版本：aom 3.13.1、archive 3.8.2、cairo 1.18.4、cgif 0.5.0、exif 0.6.25、expat 2.7.3、ffi 3.5.2、fontconfig 2.17.1、freetype 2.14.1、fribidi 1.0.16、glib 2.86.1、harfbuzz 12.1.0、heif 1.20.2、highway 1.3.0、imagequant 2.4.1、lcms 2.17、mozjpeg 0826579、pango 1.57.0、pixman 0.46.4、png 1.6.50、proxy-libintl 0.5、rsvg 2.61.2、spng 0.7.4、tiff 4.7.1、vips 8.17.3、webp 1.6.0、xml2 2.15.1、zlib-ng 2.2.5。

cb8 已有 cmake 和 pkg-config；没有 meson、ninja，已检查的 glib/gio/gobject、jpeg/png/tiff/webp、lcms、exif、rsvg、pango 开发包均不在 pkg-config 清单。任务目录还没有可复用的 libvips prefix。MuPDF 自带的 JPEG 库不是这里固定的 mozjpeg，不能复用后声称输出字节一致。固定 Node 22.22.0 的已构建源码和头文件可用于稍后的 Sharp addon 源码编译。

`tools/src/sharp-libvips-1.2.4` 已按完整 SHA 取回；抓取日志 `logs/fetch-sharp-libvips.log`。原官方 Linux `build.sh` 会拉 Docker 基础镜像、用包管理器装工具、使用 `/root` 等全局目录，并按 nproc 扩大并发，本任务不会原样执行。拟沿用其固定依赖及补丁来源，将源码、构建和安装位置改为任务目录；meson 从纯 Python 源安装，ninja/cargo-c 及缺少的构建工具从源码安装；固定 jobs=6，与 Cargo/MuPDF 共用唯一构建锁。所有补丁、源码及依赖许可证和输出哈希需归档。

计划 prefix 为任务 `tools/vips`，Rust 原生小组件和 Linux TS Sharp addon 共同链接此 prefix，减少解码器/编码器版本差异。先完成当前 14351f4e 与 MuPDF 队列，再进行此构建。正式能力必须对照同一固定 TS 的真实图像运行验证：JPEG/PNG/WebP 大小预算阶梯、旋转方向、损坏或截断修复、缺组件、取消及输出上限；构建成功本身不能替代这些行为证据。

实际工具准备完成 Meson 1.9.1、Ninja 1.13.1、gperf 3.3、cargo-c 0.10.18；cargo-c 首次缺 OpenSSL 开发库失败，改用该固定源码自带 `vendored-openssl` feature 源码编译成功，保留两个日志目录。libvips 第一次在 GLib 下载遇到 curl 35 连接重置，重试又出现 curl 56；首败均保留在 `logs/f2131756-vips-build` 和 `logs/f2131756-vips-resume-glib`，未删除已成功依赖。用相同上游 recipe、固定环境和任务锁从明确失败边界继续，后续日志 `logs/f2131756-vips-resume-{glib2,xml}`；每次恢复脚本保存 SHA-256。

GNOME 源站在此网络持续重置连接，因此四份源码从 OSU 的 BLFS 源码镜像获得，逐一匹配官方发布校验和：[GLib 2.86.1](https://download.gnome.org/sources/glib/2.86/glib-2.86.1.sha256sum)、[libxml2 2.15.1](https://download.gnome.org/sources/libxml2/2.15/libxml2-2.15.1.sha256sum)、[Pango 1.57.0](https://download.gnome.org/sources/pango/1.57/pango-1.57.0.sha256sum)、[librsvg 2.61.2](https://download.gnome.org/sources/librsvg/2.61/librsvg-2.61.2.sha256sum)。实际镜像地址、哈希及原地址在 `logs/vips-gnome-mirrors/manifest.json`，GLib 补充记录在 `logs/f2131756-vips-resume-glib2/mirror.txt`；`build-vips.sh` 已加入这四项固定哈希的显式镜像回退，其他来源不套用未核实镜像。

Sharp 构建工具按 Sharp 0.34.5 自身 devDependencies 的下界固定为 node-gyp 11.5.0、node-addon-api 8.5.0，单独任务前缀以 `--ignore-scripts` 安装纯 JS/头文件依赖，保存 package-lock。`build-sharp.sh` 使用已源码构建 Node 22.22.0 的本地头文件、固定 libvips prefix 和 jobs=6，禁止下载 Node headers 或 @img 二进制；结束必须实际加载 `sharp/src/build/Release/*.node`，核对 Sharp/libvips 版本并记录 addon 哈希。该 addon 构建仍排在 libvips 完成之后。

实际构建已完成：`logs/f2131756-vips-resume-cairo/script.exit` 与 `logs/f2131756-vips-finalize/script.exit` 都为 0。中间 Cairo 32 MiB 源码包首次在 120 s 收到约 31 MiB 后超时，随后断点续传完成并匹配从 Cairo 官方取得的 `cairo-1.18.4.tar.xz.sha256sum`；首败保留。libvips-cpp 共享库约 17 MiB，SHA-256 `0be6a13bfa7b96871bed6c0f9b8ff975681450a1913ffda42c968d28ec5b543f`。任务 prefix 留存 385 MiB 实际源码/补丁包、423 MiB rsvg vendor、实际 Cargo.lock、原始下载清单、恢复脚本及依赖许可证。最高 ELF GLIBC 需求为 2.38，证据 `logs/f2131756-vips-finalize/{elf-versions.txt,dynamic.txt}`；这不是其他系统/旧 libc 的发行证明。

Sharp 首轮 C++ 编译 exit 0，但实际加载 exit 127（`aom_codec_version` 未解析）；原因是固定 recipe 将依赖静态纳入共享库，global build 的普通 pkg-config 未展开 heif 私有 aom 依赖。使用任务私有 `pkg-config --static` 包装后，源码编译及实际加载均通过，版本 0.34.5/8.17.3；addon SHA-256 `80f9390f6090197e527fd4dc48e57f3a3f64cb532a478f6b43b328b7bc56ae9b`。首败 `logs/da2f48a5-sharp-build`，复验 `logs/da2f48a5-sharp-build-static`，包装脚本及哈希 `logs/sharp-static-wrapper/`。没有修改 Sharp 源码。

8fa6e677 的原生 C++ helper 编译通过（C++17、Wall/Wextra/Werror），同样需展开静态私有依赖；首轮链接失败与修正通过分别保存在 `logs/8fa6e677-image-build`、`logs/8fa6e677-image-build-static`。固定 da2f48a5 driver `check-image.mjs` 首次实际执行 37 组 TS createReadFileTool 与 helper 比较，34 通过、3 失败，manifest passed=false，日志 `logs/8fa6e677-image-component-check`。两个 rotate.jpg 预算场景 TS 成功产出 JPEG，helper 出现 tile 读取错误后误判预算耗尽；truncated.jpg 两侧均拒绝且主错误文字相同，但 helper stderr 额外输出带时间/PID 的 VIPS-WARNING。已交组件作者修正，未豁免。其它 34 项包含 PNG/JPEG/WebP/GIF、alpha/profile、预算阶梯等字节一致；完整实际矩阵以 manifest 为准。Rust 图像工具装配仍是后续产品候选，不能由组件构建代替。
