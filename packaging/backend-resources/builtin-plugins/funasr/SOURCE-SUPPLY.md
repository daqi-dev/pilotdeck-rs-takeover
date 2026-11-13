# FunASR 源码与声学模型供给候选

此候选已完成官方来源、历史身份、固定源码与模型数据准备，**已完成Mac原生源码构建、真实声学推理、stdio MCP与正式Rust loader正常调用**。现有13项桥接/supervisor测试用mock executable，不能作为声学验收。全局coverage和manifest仍保持pending；本说明不改变Windows当前不可用的限制。

## 固定身份与许可

固定TS `cc7c4e6`（2026-10-13）在 `src/extension/plugins/builtin/funasr/funasr-runtime.mjs:4-19` 固定runtime发行标签，模型却使用master，无模型字节hash。未找到现有真实模型cache/lock记录；当前仓库 `_tmp/20261113-resource-review/fixture/runtime/models` 是mock，不能复用。以下身份来自官方历史记录，并非把今天main当新功能基线。模型历史revision早于TS基线，但不能据此宣称掌握某次历史用户安装的实际缓存字节。

- [FunASR runtime标签](https://github.com/modelscope/FunASR/tree/runtime-llamacpp-v0.2.0)的annotated tag object为 `ef27da6d5332d801ede62dcba9811151d1b936ce`，2026-08-11，peel到 `500956bc331bb7edbaac58d8f84a84f28bd3d29f`。该commit根LICENSE为MIT；其 `runtime/llama.cpp/CMakeLists.txt` 固定llama.cpp `803b7fcae893e9caaee3921779628fef83ac0965`，该依赖根LICENSE为MIT。
- [SenseVoice q8历史上传](https://huggingface.co/FunAudioLLM/SenseVoiceSmall-GGUF/tree/cebc2cdd171e895d783040dbd15f10f3a76f7151)：2026-06-21；254,208,320字节；SHA256 `4ae45c94422de949b387e2e0fb10d7e14e4c42c69db30c3444ecc7d4b844b7c5`。
- [FSMN VAD历史上传](https://huggingface.co/FunAudioLLM/fsmn-vad-GGUF/tree/5644b91e991622131f287c034f2a0424ba5be928)：2026-06-20；1,720,512字节；SHA256 `1270f2559c495f4e7b6e739541151027d360761a3fda43fc147034f5719f5479`。
- 两个历史revision的官方模型card均声明Apache-2.0。发行前应随模型保留固定revision的card、来源与Apache许可文本，核查原始权重/转换归属和NOTICE；card声明本身不等于已经完成最终发行许可归集。构建脚本先保留两个源码根LICENSE；静态链接的third-party notices还须按实际构建闭包收齐。

`source-lock.json`保存上述revision、大小、LFS hash、许可与固定下载URL。原始tag/API/history/card证据暂存 `_tmp/20261113-funasr-source/`，不入库。

## 受控构建（只由root/E0分配重型槽后执行）

已复用本机现有源码构建工具，记录在 `_build/mac-native-components/records/build-tools-delivery.json`：CMake3.31.8，源码commit `dbe9d4593f4b7a2b1eb87826949f09af0268240a`，可执行 `_build/mac-native-components/tools/bin/cmake` 的SHA256为 `6fd3708a9a7a29998c7c25cada901393570200e751a1e7576f2cf8822c470175`。PATH未暴露它不代表需要重装。其他机器使用该机器已受控构建的CMake（至少3.16），记录对应身份。

在仓库根执行，路径全部隔离；为产品启动单独设置HOME和PILOT_HOME，不用真实用户cache：

```bash
export FUNASR_WORK_ROOT="$PWD/_build/funasr-source"
export FUNASR_RUNTIME_ROOT="$PWD/_tmp/funasr-acoustic/home/funasr"
export FUNASR_CMAKE="$PWD/_build/mac-native-components/tools/bin/cmake"
bash packaging/backend-resources/builtin-plugins/funasr/build-source.sh --prepare-only > "$PWD/_tmp/funasr-build.log" 2>&1
python3 packaging/backend-resources/builtin-plugins/funasr/fetch-models.py "$FUNASR_RUNTIME_ROOT"
# 仅在获得重型构建槽后另行执行：
bash packaging/backend-resources/builtin-plugins/funasr/build-source.sh --build
```

脚本只fetch两个固定源码commit，不下载任何预编译binary；CMake明确使用本地固定llama checkout和disconnected模式，编译单一SenseVoice目标，parallel固定4。安装布局沿用当前桥接的runtime/v0.2.0/platform-arch与models目录。模型下载先写.part，按固定大小和SHA256验完才发布；已有文件也重新hash，不能因存在即信任。下载254MB模型需要独立网络/磁盘安排，当前未执行。源码树已有修改或HEAD不匹配时构建停止。两脚本仅通过bash语法/AST静态检查，不声称已经构建通过。

## 真实短音频验收

音频应独立生成或明确授权供给，不能复制只读resources的meeting.wav进产品。macOS已有中文系统voice，可在隔离项目生成一句“项目测试已经完成，请在周五提交报告。”，用系统say输出AIFF，再用afconvert转为单声道16kHz PCM WAV。生成命令、voice身份、WAV SHA256、时长/采样率、参考文字全部归档；生成是否成功以及该voice是否可用仍应以实际输出为准。Linux使用受控已安装TTS或授权同一WAV数据，不另下载预编译程序。合成录音用于声学smoke，不代表真实会议准确率。

第一层直接运行源码构建的CLI：`llama-funasr-sensevoice -m <q8> --vad <vad> -a <project>/sample.wav --srt`。保存stdout/stderr、退出码、耗时、binary哈希、模型receipt、依赖链接检查（Mac otool/Linux ldd）。验收须有非空SRT、有效且单调的秒级区间、识别“项目”“报告”等参考关键字，并人工核对短句；不能只断言进程exit0。

第二层通过当前 `funasr-local-mcp.mjs --project-root <project> --runtime-root <cache>` 发真实initialize/tools/list/tools/call，等待call完成后再关闭stdin；校验text与structuredContent.segments及与CLI的内容对应，观察实际子进程close。当前工具只接受language:auto。随后以同一真实runtime分别验证不存在的模型、错误音频、project外symlink拒绝，以及真实推理中的cancel/EOF owned group清理；不按名字杀进程。

第三层由root安排正式Rust CLI/LocalGateway加载当前打包插件，实际发现并调用 `mcp__funasr__transcribe_audio`，模型编排用已有本地HTTP mock即可，不需要真实外部模型账号。录制attachment到转写再写本地纪要的实际工具事件、返回及落盘。正式loader/搬迁包调用成功之前，不把直接CLI或stdio成功标成完整后端交付。

Windows原生声学引擎可能可源码构建，但现桥接明确拒绝Windows；本候选不替换已审supervisor，也不宣称完成Windows进程树所有权或端到端支持。

## 本次prepare-only实际记录

根批准后已完成固定source fetch和模型数据下载，全部位于 `_build/funasr-source/`；没有运行CMake configure/build，没有build目录。源码两个HEAD精确匹配锁且工作树clean，按git archive HEAD字节记录hash：FunASR `ade5f6826d2cd93b6108480dc03648321872b8fdd8fd06c609ab1f451936589c`（106055680B），llama `54a4a50a18149fab630cce0f167513f69fce8eadd3ddfb784ac37177b20af121`（166993920B）。模型实际size/hash均与source-lock匹配，见runtime-cache/model-receipt.json。

历史revision的模型card与Apache2文本、FunASR/llama根MIT、llama vendored notices及FunASR miniaudio双许可文本已留存，共11项receipt文件；miniaudio选择MIT-0。该记录是source阶段归集，仍不能代替实际构建后的完整链接闭包许可核验。记录分别为records/source-license-receipt.json、license-inventory.json，副本放入runtime-cache/licenses供后续搬迁。

独立WAV由本机已安装Tingting中文voice生成，经afconvert得到单声道16-bit PCM/16kHz、65988帧、4.12425秒，文件136072B，SHA256 `f7e30fbdbc7c2b0b344a99810beb2762c69e54429cb754b1268bea9508db4fc7`。参考句为“项目测试已经完成，请在周五提交报告。”。audio-project/sample.wav及records/audio-identity.json已准备，未复制只读baseline音频。records/acoustic-contract.json保存隔离HOME/PILOT_HOME、native argv、MCP initialize/list/call和严格内容/区间/真实close断言。尚未进行识别，不能称已识别正确。

prepare初始可用磁盘16GiB，结束约15GiB；供给目录约605MiB。下载/准备进程均已退出，不占nativeheavy槽。下一步由root/E0排队执行显式--build，然后真实声学及正式Rust loader验收。

## Mac原生构建与声学实测

根分配唯一heavy槽后显式--build，源码单目标parallel4 exit0；CMake使用锁定本地llama，没有网络获取或预编译binary。构建前可用9.30GiB，configure后8.76GiB均过8GiB门，根回收其他已退出Cargo的incremental后build完成14.06GiB，原生推理后12.48GiB。FunASR固定CLI本身CPU线程数硬编码8且无threads选项，本次未改外部源码；只运行单个原生推理进程。

安装binary 3009688B，SHA256 `be54daedfe572e8399efeb7bed943f29f23b5ef768552712db9c393ecf57deb7`。`records/build-start.json`、build.log、build-result.json记录真实source/tool/lock/target身份；native-link.txt记录实际static ggml/CPU/BLAS/Metal链接，otool只见系统Framework/lib，无Homebrew动态依赖。源码许可与miniaudio MIT-0文本保留，native-link-identity.json记录本次链接闭包来源。

对既有独立WAV执行正式CLI的`-m/--vad/-a/--srt`参数，耗时0.619秒，exit0且进程已reap，真实SRT区间0.000–4.100秒；识别“项目测试已经完成，请在周五提交报告。”与独立参考逐字一致。records/native-result.json、native.stdout.srt、native.stderr.log保留实际结果、内容/时间/退出断言与音频/引擎hash。该结果证明4秒合成中文smoke的真实声学推理，不外推自然会议准确率或正式Rust全链路。heavy已归还，stdio/正式loader排队另验，Windows限制不变。

## 真实stdio及正式Rust loader正常链

light1授权下运行当前stdio MCP，实际initialize/tools/list/tools/call返回与原生CLI完全一致的中文文本及structuredContent.segments[0,4.1]，调用完成后才关闭stdin。Node/supervisor/native进程树观测、退出0和空tmp验证通过；记录 `_tmp/20261113-funasr-real/stdio/receipt.json`。引擎、GGUF和音频与上一节相同，不使用mock executable。

正式loader使用封存a001 CLI，SHA256 `74010f7bae071abbe72c9be904f0bd4835904d533f8e5ca4457cf275a58e3f90`，插件从当前独立编写资源目录搬迁到隔离bundle；只连接本地HTTP模型编排与随机端口gateway，不用真实外部模型API。第一轮driver默认projectKey为隔离general=PILOT_HOME，但把WAV放另一个未选project，真实工具ENOENT后模型严格断言失败，所有原始帧/请求/失败保留formal-first，不能算通过。第二轮将同hash WAV放在真正选中的general项目内，保留路径gate、所有内容断言与工具执行要求。

第二轮正式loader真实广告mcp__funasr__transcribe_audio，实际outer permission_request经permission_decide allow返回delivered；tool_call_finished.ok=true，模型随后收到完整中文和[0.000 - 4.100]，真实assistant文本及turn_completed.finishReason=completed通过。正式server退出0，gateway TCP拒绝后续连接。观测确切MCP56255→supervisor56295→native56296完整argv，shutdown后三者均不存在；实际perSession --user-data-dir已删除，tmp无regular文件，仅宿主空ledger-lock目录保留。记录formal-second/verified-receipt.json、frames.json、requests.json、tool-results.json与processes.json，路径均在 `_tmp/20261113-funasr-real/`。postcheck最初按PID去重误取退出态短command导致验证脚本首败，改为保留同PID最长已记录argv后通过；原始观测未改写，未重跑产品掩盖失败。

该验证闭合Mac26.6本机正常stdio与正式Rust加载/权限/工具/结果/退出合同；完整attachment自动触发到写纪要、真实声学cancel/EOF/错误模型负例、旧Mac deployment target和Windows仍未在本批执行，不外推为通过。normal链结束light1已归还，无core源码改动或新Cargo构建。

## 真实声学错误、取消、EOF与附件纪要链

基线依据固定TS tests/cli/funasr-meeting-e2e.spec.ts：附件type:file/name/path/audio-wav及channel metadata进入submitTurn，scripted Agent从真实输入的registered attachment path选择ASR，再write_file落盘纪要；模型编排是脚本，声学引擎真实执行。该测试源码要求转写时间戳、摘要/行动与实际文件内容。本批采用独立短中文WAV与本地HTTP脚本模型，不复制基线音频或声称语义模型质量；本批还通过read_skill实际加载独立编写的两项recipe。

三项真实负例均通过，证据 `_tmp/20261113-funasr-real/fail-cancel/`：坏WAV是项目内存在的普通文件，实际native运行后返回nonzero的FunASR transcription failed诊断；取消/EOF音频是独立4秒WAV重复60次（约247秒），触发条件为实际观测到native与supervisor而非固定延时。gate保存PID/PPID/PGID/start/argv，断言native.ppid=supervisor.pid且native.pgid=supervisor.pgid=supervisor.pid。cancel通过真实notifications/cancelled得到transcription cancelled；EOF关闭stdin、无call响应后实际MCP退出0。每项验证所有观测PID和该owned PGID均已消失，不以主进程退出代替树清理。该gate证明正在运行真实native任务，不声称精确定位到声学图的某个内部计算阶段。

附件纪要链一次通过，证据minutes-first/verified-receipt.json（同上_tmp根）：a001封存CLI SHA74010f7b...、同engine/model/WAV，submit_turn携带真实附件。provider请求中读取实际 `- meeting.wav: <registered-path>` 并断言等于隔离项目文件，系统prompt实际广告两skill；随后四工具依次为read_skill(meeting-recorder-assistant)、read_skill(audio-transcription)、mcp__funasr__transcribe_audio、write_file，实际结果均ok。两项真实outer permission都allow并收到delivered。模型使用真实转写返回写纪要，含逐字中文、[0.000 - 4.100]、总结及周五提交报告行动，未捏造负责人；实际文件SHA256 `cec0f9108e027024986d6ccd146394b8a038843dea36f231e1d453cfe776c82b`。final assistant与turn_completed.completed均通过，server exit0/socket关闭、native69705→supervisor69704→MCP实际树全退出，perSession目录删除、tmp无regular文件。

本批源代码/协议gate未改，没有Cargo或重构建，light1已归还。当前Mac实际engine最低target26.6、旧Mac目标未重建、Windows进程树owner/平台执行仍不可用；资源诊断文案尚不能声称固定TS逐字等价，沿用原coverage待核口径。合成短音频不替代真实会议/多语种准确率评测。
