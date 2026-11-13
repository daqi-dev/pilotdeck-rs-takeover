# 后端发行装配

只使用 Python 3.11+ 标准库；不运行 Cargo、不下载组件、不依赖 Node，也不发布任何内容。构建仍由主协调者/E0的单独队列执行。StaffDeck 已按本期决定暂停，包清单不声明其可用。

`core` 包含 Rust CLI 和独立 sidecar；`full` 另外必须包含声明支持的 MuPDF、libvips 图像组件及它们的源码和许可材料。这里的 `full` 表示组件组合，不表示所有后端入口已经验收或内建技能资源已经齐备。资源缺口记录在 `manifest.json.pendingDelivery`；不能删除这个记录来宣称完整交付。

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
  --variant core --out /path/to/new-core-output
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
