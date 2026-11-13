# 后端发行装配

只使用 Python 3.11+ 标准库；不运行 Cargo、不下载组件、不依赖 Node，也不发布任何内容。构建仍由主协调者/E0的单独队列执行。StaffDeck 已按本期决定暂停，包清单不声明其可用。

`core` 包含 Rust CLI、独立 sidecar及必需的source-built ripgrep15.0.0/PCRE2 10.45 JIT组件；`full` 另外必须包含声明支持的 MuPDF、libvips 图像组件及它们的源码和许可材料。这里的 `full` 表示组件组合，不表示所有后端入口已经验收或内建技能资源已经齐备。资源缺口记录在 `manifest.json.pendingDelivery`；不能删除这个记录来宣称完整交付。

已支持的装配目标名称为 `aarch64-unknown-linux-gnu`、`x86_64-unknown-linux-gnu`、`aarch64-apple-darwin`、`x86_64-apple-darwin`。每个实际二进制检查 ELF/Mach-O 架构；支持目标名称不等于已有该目标构建证据。现有 Linux ARM64 组件实际要求 GLIBC 2.38；macOS ARM64 组件已完成源码构建、固定 TS 组件对拍和搬迁验证，记录的最低系统版本为 macOS 11，但尚未在 macOS 11 真机运行验证。

## 构建输入

在已固定且构建成功的 checkout 中，由构建负责人生成完整 `cargo metadata --locked --all-features --format-version 1`，不能使用 `--no-deps`；还须覆盖 Cargo.lock 的所有 registry/git 包，缺失即拒绝。曾有普通 metadata 只含270包，遗漏锁文件中的6个可选依赖，因此不能用于发行。记录必须保留构建命令、退出码、工具链/目标信息和源码身份。采集元数据须在同一依赖缓存仍可读取时完成许可证归集。

```sh
python3 scripts/backend-release/record.py \
  --source /path/to/checkout \
  --target aarch64-unknown-linux-gnu --profile release \
  --binary pilotdeck=/path/to/checkout/src/target/release/pilotdeck \
  --binary pilotdeck-agent-loop-sidecar=/path/to/checkout/src/target/release/pilotdeck-agent-loop-sidecar \
  --evidence /path/to/build/cargo.command \
  --evidence /path/to/build/cargo.exit \
  --evidence /path/to/build/platform.txt \
  --out /path/to/new-build-record.json

python3 scripts/backend-release/assemble.py \
  --source /path/to/checkout --build-record /path/to/new-build-record.json \
  --cargo-metadata /path/to/build/metadata.json \
  --variant core --ripgrep /task/tools/ripgrep --out /path/to/new-core-output
```

完整组件包在上述命令上使用 `--variant full --mupdf /task/tools/mupdf --image /task/tools/vips`。image 前缀必须同时拥有 helper、共享库、`share/source` 与 `share/licenses`。MuPDF同样需要源码和许可证。完整组件目录按原生查找规则放到 `components/{mupdf,image}`，不依赖环境变量指向构建机器。

core/full可显式添加 `--skills DIR`，`--plugins DIR`仅接受纯skill插件（例如windows-skills）；含MCP服务或hook的插件必须作为明确选择的独立addon归档。打包器拒绝把本 checkout 的参考 `resources` 目录当作资源输入；不能从 TS 参考目录复制技能、插件或业务实现。是否已覆盖所承诺的资源仍须另行验收，存在目录本身不是覆盖证明。

`pilotdeck-gateway` 可作为第三个 `--binary` 输入，但只有其默认入口已完成 native 切换与独立验收后才能纳入候选；不能把旧默认透明代理装成正式后端。

默认拒绝 dirty 产品源码和 debug 构建。`--development` 仅供开发包验证，明确写入 classification，不可当成 release 构建证据。`record.py`记录的是现有文件与源码快照，并不能自行证明二进制来自该源码；构建负责人须在实际构建结束后立即记录并归档原始构建证据。

## 内容与校验

输出包含解包目录、确定性 `.tar.gz` 及归档 SHA-256。包内有：

- `manifest.json`：每个文件的大小、权限、SHA-256及软链接目标，包范围、资源缺口与组件身份。
- `SHA256SUMS`：包括manifest在内的全部普通文件；外层归档哈希仍须通过可信交付渠道核对，不把内部清单当签名。
- `build.json`：源码提交/dirty快照、二进制哈希及构建证据路径。主机绝对二进制路径不会进入此文件。
- `dependencies.json`与`licenses/dependencies/`：完整锁定Rust依赖、来源、checksum、声明许可和实际许可/notice文件；缺材料立即失败。
- `native-dependencies.json`：ELF/Mach-O动态依赖、搜索路径、ELF GLIBC或Mach-O最低系统记录。非系统依赖必须随包提供，构建机器绝对RPATH不允许。
- `source.tar.gz`：显式白名单产品源码、Cargo.lock、原生helper和构建/发行脚本；不包括参考仓、私有计划、状态、凭据、target和运行数据。

归档将文件顺序、uid/gid、权限及mtime固定；gzip头也使用提交时间，无宿主文件名。相同输入得到相同归档字节，不承诺未经验证的跨编译器二进制可复现。

```sh
python3 scripts/backend-release/verify.py /path/to/extracted/package
python3 scripts/backend-release/smoke.py /path/to/extracted/package --out /path/to/new-smoke-output
python3 scripts/backend-release/test_release.py
```

smoke将完整包复制到新目录，使用隔离HOME、项目和临时目录，清空继承环境，PATH只含包及拒绝执行的Node/npm等探针。它通过CLI正式server/WS入口、动态loopback模型服务运行文本读取、PDF和图像读取，要求工具回执到达模型及真实最终完成。core必须明确报告缺组件；full必须把完整 PDF document/data URI 解码后与测试 PDF 逐字节核对，并将 PNG 输入规范化得到的 JPEG 模型图像与固定 TS [golden](fixtures/read-file-rgb1x1-jpeg.json) 逐字节核对；该 golden 的实际输出为 269 字节 JPEG，而不是原始 PNG。默认native验证时，包副本的独立sidecar被临时替换为失败探针，任何Node/sidecar调用都使smoke失败；原副本随后恢复并重新核对包清单。独立sidecar还单独执行`--version`；完整stdio/TCP协议验收由R2记录，不能用版本命令代替。

原始WS帧、模型请求、服务日志及成功/失败结果全部保留。只回收该脚本启动的进程组，不使用固定18789、不访问真实模型或账号。严格的“参考仓不存在”环境验收应由E0在只挂载发行包的隔离环境运行本脚本；本机搬迁证明不冒充完整文件访问审计。

## 默认核心与显式插件附加包

`full`保留既有参数含义：在native core上增加源码构建MuPDF与image两个组件，不表示浏览器、ASR、模型或外部CLI齐备。core/full均可包含纯资源skills；文档出现外部工具不等于包内供给。`resourceContract`记录实际skill/plugin名单、默认发现语义、Node发现依赖、附加项与外部依赖是否包含。默认loader仍对发现的builtin缺省启用，打包器通过拒绝可执行插件进入默认资源root保证布局。

```sh
python3 scripts/backend-release/addon.py \
  --plugins packaging/backend-resources/builtin-plugins \
  --plugin browser-use --out /absolute/NEW-browser-addon \
  --source-date-epoch 0
```

附加归档不含核心二进制，也不声称附带Node、Chrome、ASR引擎/模型或独立装配runtime。它只归档显式选择的plugin资源和许可；需按manifest补齐实际外部依赖后显式安装builtin-plugins内容，发现后插件会默认启用。browser公开依赖的独立离线装配方法见其runtime/README.md；该输出与资源归档不同，需单独记录与最终搬迁验证。ASR依赖也须独立记录来源、模型许可、平台与推理证据。不得用宿主绝对路径或复制核心binary冒充包内供给。

封存d0验证：空builtin root、拒Node PATH的普通native turn零Node尝试；当前browser/funasr root相同turn触发两次Node启动，即使turn最终完成仍不符合零Node合同。原始证据`_tmp/20261113-core-no-node/`。最终same-SHA发行须从包外、无资源override、无TS参考目录重验；本轮轻量脚本测试不替代该验收。

## 明确身份的 extension runtime 供应包

`runtime-bundle.py`与上述resource addon分开。它仅接受已存在、已取证的runtime，不运行下载、npm安装、源码构建或模型获取；包内不包含核心CLI、Node、Chrome或Python。build前检查至少8GiB空闲，归档压缩以nice+10运行。

先`record --kind browser|funasr --input ROOT --identity NEW.json --os OS --arch ARCH --minimum-os VALUE --receipt FILE ... --recipe FILE ...`生成精确身份；至少一个实体receipt和一个源码recipe必须提供。record只记录事实，并不等于审阅通过。审阅者核对source/lock/license/platform/receipt后，以独立核准的SHA运行：

```sh
python3 scripts/backend-release/runtime-bundle.py build \
  --kind browser --input /absolute/existing-runtime \
  --identity /absolute/reviewed-identity.json --identity-sha256 REVIEWED_SHA256 \
  --out /absolute/NEW-runtime-bundle --source-date-epoch 0
```

browser载荷位于`runtime/browser/`，含实际node_modules、package/lock/sources、公开依赖LICENSE/NOTICE与原装配manifest。保留现有协议，显式设置`PILOTDECK_RUNTIME_ROOT=<moved-package>/runtime/browser`。相邻自动探测未在此合同中承诺。Node及所选Chrome是独立外部工具，headless/channel仍由显式plugin配置决定。

FunASR载荷位于`pilot-home/funasr/`。用户显式选择安装至`PILOT_HOME/funasr`，保留现有`runtime/v0.2.0/darwin-arm64/llama-funasr-sensevoice`与`models/{sensevoice-small-q8,fsmn-vad}.gguf`路径。安装不得覆盖已有用户目录；先在全新隔离PILOT_HOME验证再决定安装。此脚本不替用户写home、不修改plugin启用设置。当前native身份由Mach-O直接解析为darwin/arm64、minOS26.6.0，禁止把旧macOS声明填低；Windows与其他native格式当前不支持此供应脚本的native身份核验。Node、Python3、macOS系统framework依然外部。包内包括引擎/模型、源锁、模型来源与许可、原生依赖许可、实际构建/声学receipt和可复现源码recipe，原始记录的历史pending字段不被篡改，后续收据分别附带。

两个供应包均有独立manifest、runtime-identity、exact-file清单、checksum与archive摘要，可由现有verify.py校验。来源receipt与recipe实体归档在provenance中，不要求安装机器访问旧_tmp路径；这些raw历史文本中的主机路径仅是证据，不是加载映射。外部核准的identity SHA与包内去主机输入路径的identity副本用途不同，二者不混用。

本轮实际供应包在`_tmp/20261113-core-no-node/{browser-runtime-bundle,funasr-runtime-bundle-second}/`：移动解压verify分别199/34payload项通过；移动browser runtime真实stdio catalog23工具并自然退出。移动ASR引擎与原引擎`--help`均exit1，除可执行路径外实际usage字节相同；首个driver误期望exit0的失败保留。未重跑搬迁后声学/Chrome/正式loader，不把这些轻量probe称为最终same-binary发行验证。

## core/full必需的原生搜索组件

两种variant均要求显式`--ripgrep PREFIX`，布局固定为`components/ripgrep/bin/rg`（Windows采用rg.exe的路径约定，但Windows构建/发行仍须单独证明）。full在此基础上增加MuPDF/image，不把必需rg当可缺的optional工具。资源addon/runtime归档不受这一默认包合同影响。旧无rg的核心包不能继续作为完整独立发行证明；旧smoke原证保持历史身份，不覆盖重判。

PREFIX须由受验源码构建过程产生，不允许从宿主PATH复制未知rg或下载预编译bin。`share/source/`包含rg/PCRE2锁定源码、所需vendor及可复现recipe；`share/licenses/`包含相应许可。构建方同时生成`share/ripgrep-build.json`：

```json
{
  "schema": 1, "name": "ripgrep", "version": "15.0.0",
  "target": "aarch64-unknown-linux-gnu", "sourceBuilt": true,
  "pcre2": {"version": "10.45", "jit": true},
  "binary": {"path": "bin/rg", "sha256": "ACTUAL_SHA256"},
  "buildEvidence": [
    {"role": "rg-command", "path": "share/evidence/rg-command.txt", "sha256": "ACTUAL_SHA256"},
    {"role": "rg-exit", "path": "share/evidence/rg-exit.txt", "sha256": "ACTUAL_SHA256"},
    {"role": "pcre-command", "path": "share/evidence/pcre-command.txt", "sha256": "ACTUAL_SHA256"},
    {"role": "pcre-exit", "path": "share/evidence/pcre-exit.txt", "sha256": "ACTUAL_SHA256"},
    {"role": "platform", "path": "share/evidence/platform.txt", "sha256": "ACTUAL_SHA256"},
    {"role": "version", "path": "share/evidence/version.txt", "sha256": "ACTUAL_SHA256"},
    {"role": "recipe", "path": "share/evidence/build.sh", "sha256": "ACTUAL_SHA256"}
  ],
  "sourcePins": {
    "ripgrep": "3a612f88b805e14aef45bfa43e25a54abc6297fc",
    "pcre2": "2dce7761b1831fd3f82a9c2bd5476259d945da4d",
    "sljit": "e51eabbfb8eabc6526f56e4e88b29fb10d1ee048",
    "Cargo.lock.sha256": "257bfa1eeacfa4d8222e2c50f9d2f8d15ef07e96adee46eaa87839f3b6cfa3b4"
  },
  "distribution": {
    "sources": {"ripgrep": "ENTRY", "pcre2": "ENTRY", "sljit": "ENTRY"},
    "licenses": {"ripgrep": ["ENTRY"], "pcre2": ["ENTRY"], "sljit": ["ENTRY"]},
    "cargoLock": "ENTRY", "vendor": "ENTRY"
  },
  "files": []
}
```

`files`必须为完整common.inventory(PREFIX)结果，排除record本身，而非示例空数组；所有来源archive、vendor、锁、许可与原始证据均纳入exact-file闭包。记录声明不自行证明source build发生；构建负责人必须保留实际命令/退出码/版本/链接/架构与源码身份，包装器核字节/目标及搬移后的完整闭包，不执行构建。现有native-dependencies审计也检查rg动态依赖，PCRE2应静态链接，未供给非系统库必须拒绝。

assemble在复制前验证版本/PCRE2 JIT/target/实际binSHA、执行权限、source/license非空、角色明确且路径互不重复的七份构建证据及exactinventory，复制后复验。core/full manifest明确requiredComponents=[ripgrep]并绑定component与buildRecord摘要；verify除普通清单/checksum外再次核必需组件，缺件即失败。

原有smoke在同一native业务任务里保留3个read_file调用和既有PDF/image判据，增加真实grep内容匹配与glob文件匹配。PATH不提供rg/Node，不设置PILOTDECK_RIPGREP_PATH；只允许搬移包相邻component成为默认引擎来源。收据包含搬移rg--version实际输出、15.0.0/PCRE2 10.45 JIT、二进制/buildrecordSHA；两个工具都必须有成功tool_call_finished且实际匹配进入provider。当前新增包装单测使用明确fake字节并mock架构，只证明结构与拒绝行为，不能冒称真实引擎或最终产品smoke已通过。真实sourcebuiltprefix与新same-source CLI统一由root排最终组包/搬移验证。

独审有限修正：`buildEvidence`的role与path均唯一，七role全部必需；rg/pcre退出文件必须为实际文本0或原JSON `{exit:0}`，exit1即使hash正确也拒绝。command/recipe核locked+featurespcre2、PCRE2_SYS_STATIC=0与PKG_CONFIG_ALL_STATIC=1、PCRE配置JITON/sharedOFF；version文本核实际15.0.0/10.45/JIT。PCRE2_SYS_STATIC=1会触发pcre2-sys自带10.46编译，不能使用。

上例ENTRY必须由`ripgrep_component.distribution_identity(prefix,relative)`生成的真实对象代替：普通文件为path/type=file/sha256，目录为path/type=directory/inventorySha256（sorted-key compactJSON的common.inventory摘要）。source按rg/PCRE2/SLJIT明确声明，含SLJIT子模块的PCRE源archive可被两source role引用。三source commit与Cargo.lock的已知SHA固定；actual Cargo.lock也必须匹配。许可证按固定源原文本摘要核验（rg可MIT或UNLICENSE之一，PCRE2 LICENCE与SLJIT LICENSE分别保留）。vendor必须真实随包，目录或tar源码归档须含Cargo checksum manifests；validator只读tar目录，不解包。

这些结构校验不认证任意不可信构建；真正source build仍由固定checkout、原始成功命令/日志、版本/链接与独立审查支持。首版只检查三份hash文件的缺陷与首测原证保留；新版增加且仅增加重复证据、exit1但hash正确、缺vendor三个对应负控。当前30项包装回归通过，其中fake测试明确mock source/license pin与架构，不能称真实离线重建成功。真实供应prefix另须通过同validator后才能组包。

### ripgrep vendor 的无损布局收敛

`compact_ripgrep.py` 是显式的新 prefix 转换，不修改输入或改变默认 assemble/verify 行为。原 prefix 必须先通过现有源码构建记录、许可、七类证据、固定锁和 exact inventory 校验，并同时拥有展开 vendor 与 `share/source/registry-vendor-source.tar.gz`。转换逐 tar 成员核对所有展开文件的路径、内容 SHA、大小和可执行位，拒绝路径逃逸、重复成员、符号/硬链接及特殊文件；不会解包归档。

```sh
python3 scripts/backend-release/compact_ripgrep.py \
  --input /absolute/validated-ripgrep-prefix \
  --out /absolute/NEW-ripgrep-prefix \
  --target aarch64-apple-darwin
```

新 prefix 仅省去重复的展开 vendor，保留原 tar、锁、source pins、许可、recipe 与全部原始构建证据；原 vendor role 已指向该 tar 时保持原 role 不变。`share/compaction/` 保留原 build record 和逐文件覆盖证明。新 build record 仅在原 role 为目录时将其指向原 tar，并重算 exact inventory，随后再次运行原组件校验。输出路径已存在即拒绝，不覆盖旧组件或发行包。新 prefix 必须作为新的组件身份参与后续 assemble、verify 与搬移验收，不能沿用旧包 hash，也不能把源码层从完整体积指标中剔除。本转换不宣称已达到 full 体积目标。

### librsvg 完整 vendor 的归档与离线恢复

`compact_librsvg.py compact` 将已有 image prefix 转为全新 prefix，只把展开的完整 `rsvg-vendor` 改为指定 SHA 的保真 tar.gz。它核对固定 rsvg Cargo.lock、原 source archive、逐 crate/package/checksum 文件、归档内每个文件的 SHA/大小/可执行位，保留全部许可证、helper、运行库和原构建/source provenance。输出已存在即拒绝，旧 prefix/full 不改；公共 assemble/verify 规则不变。

```sh
python3 scripts/backend-release/compact_librsvg.py compact \
  --input /absolute/source-built-image-prefix --out /absolute/NEW-image-prefix \
  --target aarch64-apple-darwin --vendor-archive /absolute/rsvg-vendor.tar.gz \
  --archive-sha256 ACTUAL_REVIEWED_SHA256
```

新 `share/source/rsvg-vendor-archive.json` 独立绑定归档与完整文件清单，既有 `source-distribution.json`、`vendor-manifest.json`、锁和旧配置原字节保留，不改其 schema；其中旧 offline metadata 收据仍只是原构建证据。`BUILD-MACOS.md` 追加恢复步骤且保留原文副本。重建前运行随源码分发的独立 `restore-rsvg-vendor.py restore --source SOURCE_DIR --out NEW_BUILD_INPUT`，它先只读验证全归档，再安全恢复至全新目录并写 `cargo-home/config.toml`，继续使用相同 vendored-sources 配置且强制 `net.offline=true`。原 source-cache 固定下载/补丁 recipe、锁、许可和本地平台适配仍须照原步骤使用；恢复不会下载或启动编译器。

转换拒绝缺许可、源码/锁/checksum 不符、损坏归档、路径逃逸、重复路径、符号/硬链接和特殊条目；失败仅移除本次创建的输出。归档验证与恢复控制不代表重新执行离线编译、最终 full 组包或搬移业务。完整体积分母仍包括归档与证明材料，不宣称已达到体积目标。
