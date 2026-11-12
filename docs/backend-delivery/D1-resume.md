# D1 恢复：发行验证

- 任务：D1；恢复起点 `e8618392e7e4eb24c227eedd28e14c84237ca8a4`；负责人 D1；交付提交待主协调者集成。正式入口是发行包 `bin/pilotdeck` 与独立 sidecar。StaffDeck 本期延期。
- 五层状态：接口进行中，实施进行中，接线待验证，验证进行中，交付待验证。现有脚本与资源布局见 [D1.md](D1.md) 和 `scripts/backend-release/README.md`；本轮未改 Rust、Cargo、CLI 或外部行为。
- 2026-11-13 macOS 本机：`python3 -m py_compile scripts/backend-release/*.py` 成功，`python3 scripts/backend-release/test_release.py` 8/8 成功。它们是装置检查，不是发行包运行证据。
- 复用 `_build/backend-validation/20261113-fourth-precheck/metadata-resolved.json`（270 包）及当前锁文件直接执行 `dependencies`，许可文件扫描完成后，完整性检查因锁文件另有 6 个未出现的 registry 包而失败；原始日志及退出码在 `_tmp/20261113-d1-resume/license-probe/`。缺项为 equivalent 1.0.3、hashbrown 0.17.1、relative-path 2.0.3、rquickjs、rquickjs-core、rquickjs-sys 0.14.0。以 ea6d8e71 锁文件重复也同样失败；记录在 `license-probe-historical/`。因此原 270 包 metadata 不是完整锁依赖证据。
- 独立诊断在内存中只将锁清单限制于这 270 个已观察包，执行相同归集函数，结果 270/270 文件归集完成，15 包使用经固定版本、源码 commit 和 SHA-256 验证的补充材料；r-efi 5.3.0、6.0.0 收到各自 AUTHORS。证据在 `license-probe-270/`；此诊断刻意绕过 6 个缺项，不能作为实际包或许可证完整通过的依据。此前 `alloc-stdlib` 首次失败和下载首次 TLS 失败仍保存在 `_tmp/20261113-D1/`。
- 正式组包前需要固定同一 clean 集成 SHA，在同一机器与依赖缓存中生成完整 `cargo metadata --locked --all-features --format-version 1`，仍以脚本锁文件集合检查为准；同时获得 release 构建日志、退出码、target/toolchain/platform 和两个可执行文件哈希。metadata 或构建不完整时不可组包。README 给出 `record.py`、`assemble.py`、`verify.py`、`smoke.py` 命令，先 core 后 full；Linux 的 smoke 须在只挂载包的隔离环境运行。构建队列由 E0/root 控制，本轮未运行 Cargo 或产品命令。
- 组件目前只有此前 E0 记录的 Linux ARM64 MuPDF 1.27 和 libvips 8.17.3/helper；其源码、许可、实际 prefix、动态库和平台约束须在组包时核验。历史报告要求 GLIBC 2.38；macOS、x86_64 均无本轮发行证据。full 指组件组合，不表示完整产品验收。
- 内建资源仍是交付缺口：Rust server 从包根 `skills` 和 `builtin-plugins` 发现资源，原生程序集加载插件及 MCP 路径；当前无已核准、可发行的等效资源集合。固定基线涉及 browser-use、funasr、windows-skills 以及文档类技能和辅助脚本；JS 函数加载兼容不在本期范围，不能把参考目录直接打包，也不能用空目录声称覆盖。最小需求是逐项列出基线实际提供的技能与插件、入口及必要的外部运行时/平台，确认哪些属于本期后端能力；独立编写可发行资源或提供经过许可的独立外部 MCP 实现，并从正式 CLI 验证加载、禁用、调用、缺失和平台限制。具体覆盖及许可核验后才能移除 manifest 的 pendingDelivery。
- 未完成：最终 SHA 的完整许可归集、release 构建及固定身份、core/full assemble → verify → smoke、非参考仓隔离验收、平台矩阵、资源、性能和组合路径。未发版、推送、部署或提交。

## 276 包归集修复

E0 随后提供 `e8618392e7e4eb24c227eedd28e14c84237ca8a4` 的 `cargo metadata --locked --all-features --format-version 1`（276 包，原始文件 `_tmp/20261113-e0-resume/metadata-all-features.json`）。将其中 cb8 的 manifest 路径映射到本机同版本依赖缓存与工作区后，首次正式完整集合归集因 `rquickjs-core 0.14.0` 缺许可失败，原日志在 `license-probe-276/`。发现 `rquickjs-sys` 含内嵌 QuickJS 的 LICENSE，但其存在会遮盖 Rust 包装层缺失的 MIT 许可。已基于两包 `.cargo_vcs_info.json` 的同一固定提交，从官方源码取得许可，记录 URL、Git blob 和 SHA-256；修改归集器使显式补充材料与内嵌许可共存，而非只在完全无其他许可文件时添加。新增测试覆盖此问题。

在新目录 `license-probe-276-fixed/` 使用同一份路径映射 metadata 及当前完整 Cargo.lock 归集成功，共 276 包；`rquickjs-sys` 同时保留 QuickJS、wasi-libc 和包装层材料。语法检查及 9 项装置测试通过。这是 e861 的本机缓存许可材料归集验证，不是最终固定集成 SHA 的发行装配；最终构建机仍须用自身原始 metadata、缓存和 clean 源码重做。E0 已确认组件 prefix 为 `/home/clawbox-08/ws/pilotdeck-rs-cc2c/tools/mupdf` 和 `/home/clawbox-08/ws/pilotdeck-rs-cc2c/tools/vips`，实际身份记录 `_tmp/20261113-e0-resume/{mupdf-identity,image-identity}.txt`。release 构建尚未开始。

## 当前 MD5 依赖增量

根为WeCom引入缓存中的固定`md-5 0.10.6`后，使用全features完整metadata在 `_tmp/20261113-d1-resume/license-probe-277/` 再次执行同一`dependencies`归集，277个锁定registry包材料完整，exit0。此结果属于当前工作候选和本机缓存，不替代最终固定提交/构建机的发行归集；commands后续依赖抽取若改变锁定图仍需复核。
