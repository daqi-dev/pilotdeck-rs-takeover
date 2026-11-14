# Linux AArch64 本地 ASR runtime 部署

本说明定位到已经存在并验证的源码构建引擎、两份模型、许可与构建/loader 收据。它是可选 `funasr` 插件的外部运行时供给说明，不是新建大归档，也不把模型或引擎提交到 Git。当前证据支持 Linux 正常声学识别和正式 Rust loader 的权限、工具结果、模型续轮及自然退出；范围限制见末尾。

## 已存在的交付物

已核机器为 Ubuntu AArch64/glibc。供给主机工作区根为 `/home/clawbox-08/ws/pilotdeck-rs-cc2c`，现有 runtime 位于：

```text
_build/funasr-linux46/runtime-cache/
├── runtime/v0.2.0/linux-arm64/llama-funasr-sensevoice
├── models/sensevoice-small-q8.gguf
├── models/fsmn-vad.gguf
├── model-receipt.json
├── licenses/                 # 原许可正文、模型卡、许可收据与历史 SUPPLY-NOTICE
└── source-records/           # 原 source/license 来源记录
```

引擎 SHA256 为 `46cfa8e2a98fa8e9b1eeda5065ede051d682262e7693a358fdb848887b68c018`，2402728 字节，ELF64/AArch64；实际 build 脚本安装时设置 0755。两模型分别为 254208320 与 1720512 字节，精确 SHA 和历史 revision 见同目录 `linux-asr-runtime.manifest.json`。模型实际可读已由直接引擎和正式 loader 证明；未另外采集完整 runtime tree mode/stat 清单，不宣称所有文件模式均已独立核实。

这份 runtime tree 和既有 `transfer-source-data.tar` 是不同物料。后者 SHA `cbe9c18413fdb553380c225edad89e20f779f0a5536959426b50332f5a8578e3`，347893760 字节，是源码 Git bundles、模型、音频和许可的 source/data 输入归档，不是 runtime 安装包，也不含 Mac 可执行程序。两源码 clean checkout 固定于 FunASR `500956bc331bb7edbaac58d8f84a84f28bd3d29f` 和 llama `803b7fcae893e9caaee3921779628fef83ac0965`；原构建入口及 configure/build 原 argv 保存在 `_build/funasr-linux46/run-build.py` 与 `records/`，不是本说明重新执行的构建。

## 安装定位与前提

先安装正式的 `funasr` 可选插件及其 Node MCP/supervisor 资源，确认实际启用/发现该插件。插件资源不由本 runtime tree 替代；受测 MCP 资源在 `builtin-plugins/funasr/`，Node 22 与 Python 3 需独立可用。Rust registry 将 runtime root 解析为当前 `PILOT_HOME` 下的 `funasr`；`PILOT_HOME` 与音频所在项目 cwd 可以不同，不能凭项目名猜目录。插件逐层定位的固定引擎/模型相对路径就是上表。

交付方应将**完整 runtime-cache 子树**连同全部 licenses/source-records 和本次 final NOTICE/manifest 提供给安装者，再离线验证清单；另附 manifest.receipts 列出的17份已有构建/声学/loader元数据原件（建议独立 provenance/ 目录）、原 source supply manifest 及本说明。现有原件位于 runtime-cache 的父级 records/formal-driver/formal-first/formal-owner-first，不会随只复制 runtime-cache 自动带上；必须逐项配齐，不能只交来源 URL。源码 bundles 保留在既有 source/data 供给中，不需要为本次文档再打大包。以下为在已有供给主机上由安装者显式选择隔离目标的示例，不是本次已执行的安装：

```sh
ASR_SOURCE=/home/clawbox-08/ws/pilotdeck-rs-cc2c/_build/funasr-linux46/runtime-cache
ASR_TARGET="${PILOT_HOME:?先选择安装目标 PILOT_HOME}/funasr"
test -d "$PILOT_HOME" || exit 1
test ! -e "$ASR_TARGET" || exit 1
cp -a "$ASR_SOURCE" "$ASR_TARGET"
chmod u+rx "$ASR_TARGET/runtime/v0.2.0/linux-arm64/llama-funasr-sensevoice"
chmod u+r "$ASR_TARGET/models/sensevoice-small-q8.gguf" "$ASR_TARGET/models/fsmn-vad.gguf"
```

跨主机由已有受控传输提供同一 tree，目标不得覆盖未知旧安装；模型不通过 Git 分发。复制 `LINUX-ASR-NOTICE.txt` 和 `linux-asr-runtime.manifest.json` 到目标 `licenses/` 后，保留旧 `SUPPLY-NOTICE.txt` 原字节。Final NOTICE 仅将其“source-stage/link closure pending”状态更新为已存在实际 build/ldd 收据，不抹去历史状态。

依赖的 OS 动态库包括 loader、libc、libm、libgcc_s、libstdc++、libgomp；**这些库没有捆绑在此 runtime tree 中**。本机实际 ELF/ldd closure 无 not-found，包归属与 SHA 见 manifest 对应 OS dependencies。目标机须独立满足 AArch64/glibc/OpenMP ABI；不能把 Ubuntu 本机 ldd 成功当任意 Linux 可运行，也不应为求成功直接复制整个宿主库目录。CMake/GCC/G++/make 是既有 OS 构建工具；Node/Python 是执行 MCP/supervisor 的另行供给，不包含在 ASR engine 的 ELF closure 声明里。

## 已有验证与限制

直接引擎实际识别一份独立合成的约4秒中文音频，输出 SRT。正式 loader 使用 candidate `c88650f6d89bd839d900e6256aa65bdca82e0b3d` 的 Linux release CLI，SHA `8ec809aee94c5f0ace1b490eb7bc49c1239e509849d7e90c7f549dce14cb3b93`，完成一次真实 `mcp__funasr__transcribe_audio` 审批/ack、工具成功、后续本地模型请求携结果及最终 completed；没有真实模型 API。引擎、两模型、音频及来源文件前后 hash 收据完整。

正式进程原观察有 MCP→Python supervisor→native engine 的 PID/PPID/birth/argv 链。原 postflight 仅按规范 runtime-cache 路径过滤，漏掉了 `pilot/funasr` 软链接别名，故 `engineObservedCommands=[]`；新增 `loader-alias-aware-observation.json` 是读取既有原件的派生纠正，不是产品重跑。原空数组保留。没有采到正式 native 运行期间 `/proc/exe` 实际文件 hash；不能借另一次 direct 引擎 PID补齐该限制。

当前不由本批证明 Linux cancel/EOF、坏模型、真实自然语音准确率、附件自动触发会议纪要、Windows 或其它 Linux ABI。Mac 的历史成功不替代这些 Linux 证据。完整材料出处和本机镜像路径见私有交付 manifest；本次只整合说明与哈希，不重新构建、运行引擎或制作新大归档。
