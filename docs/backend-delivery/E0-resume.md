# E0 恢复验证记录

任务 E0；负责人：本轮 E0 验证智能体。恢复起点与本批固定源码 `e8618392e7e4eb24c227eedd28e14c84237ca8a4`。无产品源码修改，无自行提交。StaffDeck 延期。

## 身份与隔离

- 本机 `/Users/da/ws/pilotdeck-rs-cc2c`；cb8 任务根 `/home/clawbox-08/ws/pilotdeck-rs-cc2c`，实际 Git checkout 为其 `checkout/`。
- 旧流水线父 PID 819567、run.sh 819568、nextest 819617 均已退出；先收结果后才用 Git bundle 更新固定源码，未推 Git 远端。
- cb8 唯一重型槽，Cargo 参数 `-j 6`；产品命令隔离 HOME/PILOT_HOME。新批不导出 CARGO_BUILD_JOBS（该环境变量使 unsafe_boundary 守卫拒绝运行），使用 umask 022。
- 平台 Linux aarch64；rustc/cargo 1.99.1，Node v22.22.0；完整 platform 证据在每批目录。

## 旧流水线结果（仅属于 ea6d8e71）

源码 `ea6d8e71fd71f57c929e81301965d41e573368a4`。workspace nextest exit 100：7983 run、7977 passed（11 slow）、6 failed、742 skipped，耗时 634.704s；doc tests exit 0。远端 `logs/ea6d8e71-workspace-{nextest,doctest}/`；汇总 `logs/ea6d8e71-workspace-gates.exit` 为 `nextest=100`、`doctest=0`。本地原始日志与提取详情 `_tmp/20261113-e0-resume/old-{nextest.log,doc.log,failures.txt}`。

失败逐项保留：

| 测试 | 首次失败 | 当前归因 |
| --- | --- | --- |
| production_adapter_dispatches_to_host | factory_tests.rs:254，`#745 native transport cannot request ports` | 根分配 native_regression 处理 |
| builtin_tool_outputs_match_real_ts | PDF component unavailable | 待带组件定点复跑；旧 9-byte PDF 夹具可能与原生解析不兼容 |
| multimodal_success_branches_emit_model_visible_payloads | PDF component unavailable | 同上，夹具明确为 `not a pdf`，不能仅以补环境宣布闭合 |
| a_nested_call_runs_through_the_runtime_under_a_derived_id | glob 缺 linux-arm64 bundled ripgrep | 待固定源码与显式源码构建 rg 定点复跑 |
| compiler_enforces_gateway_boundary | CARGO_BUILD_JOBS 非空被守卫拒绝 | 后续 unset 环境，仅 Cargo `-j6` 限额，待复跑 |
| write_tools_match_real_ts | 新文件 history mode 0664，oracle 为 0644 | 疑 umask 环境；待 umask022 因果复跑 |

## 固定 e861 窄批

原目录 `logs/e8618392-resume-narrow` 因非登录 SSH PATH 无 cargo，三个 gate 均 exit127。没有产品测试执行，保留原始记录。补显式 `/home/clawbox-08/.cargo/bin` 后使用全新目录 `logs/e8618392-resume-narrow-r2`，脚本副本本地 `_tmp/20261113-e0-resume/run-narrow.sh`。

- `cargo metadata --locked --all-features --format-version 1` exit0，276 包；本地 `_tmp/20261113-e0-resume/metadata-all-features.json`。已交 D1，不能继续使用旧 270 包 metadata。
- `cargo fmt --all --check` exit0。
- `cargo clippy -p pilotdeck-gateway --all-targets -j6 -- -D warnings` exit101：`runtime/model/http/complete/google.rs:165-169` collapsible_if，已报根分配修复。
- nextest 窄批 exit100：12 run、11 passed、1 failed、7395 skipped（3.539s）。默认 native 的 mode_routing、Google complete 两项及公开 native_extensions 消费者均通过。excluded_builtin_names_and_aliases_remain_reserved_for_native_admission 失败：实际错误含 `Callback (accepted=false, stream_id=None, last_applied_sequence=None):` 包装，预期没有；已交根分配修复。

原生组件既有 prefix 为任务根 `tools/mupdf` 与 `tools/vips`；工具源码构建身份本地 `_tmp/20261113-e0-resume/{mupdf,image}-identity.txt`。MuPDF 源码身份 d3b7556577b790e9761868c314ad6fd9b6dd86a9；image helper a903ff30c1901e4ba88d8b3ee680eadb73f64cd2，vips8.17.3。这些是既有组件身份，不代替新源码二进制身份。

## 状态与下一步

接口、实现、接线由产品作者负责；本包验证进行中，交付待验证。保留首次失败，完成固定窄批后再做旧五项环境/夹具归因。不在流水线运行时切源码、不拿旧二进制验收新代码；全量与最终 release 等根排期。

## 环境因果复跑（固定 e861）

远端 `logs/e8618392-environment-replay/`，本地完整副本 `_tmp/20261113-e0-resume/e8618392-environment-replay/`；批次已结束，PID999935退出。对应实现和测试从 ea6 到 e861 无源码差异。组件 sha256：MuPDF `bf370bfe6463d644b29f466b01ad3ac8bbac194fdb53bed860cfeb6e88ee69cb`；image `1e76e87d47bf7e0496aeef1f82edc6b913bafc98b2f4336f633c751d05987bee`；源码构建 rg `8a279d0596938e22140d008271eaf175bbaa144195e708967c5c2dea4d4d0212`。

- 真实 MuPDF 下旧 builtin_tools 两项仍失败（2 run、0 passed、2 failed，exit100），错误变为 `tool_execution_failed: cannot find document handler for file .../input`；不是只补组件环境即可闭合。已通知 S1 review 按根授权修复；不修改 oracle 断言。
- 显式 PILOTDECK_RIPGREP_PATH 指向既有源码构建 rg，nested execute_code 单项通过（exit0）。
- umask022 下 write_tools_match_real_ts 单项通过（exit0），原0664/0644差异由环境解释。
- unset CARGO_BUILD_JOBS、以 Cargo -j6 限额，compiler_enforces_gateway_boundary 单项通过（exit0）。

窄批完整原始证据已同步 `_tmp/20261113-e0-resume/e8618392-resume-narrow-r2/`。当前唯一重型队列空闲；后续只在根提供新固定集成提交后验证修复，不把 e861 窄批通过项冒充里程碑全量通过。

## 独立 core 候选 977026b2

根提供的本地 ref `codex/backend-resume-core-candidate`，完整 SHA `977026b2922e7196afcafa747e7865a201a15a81`（parent e861）。先确认前批退出，再传增量 Git bundle 并 clean detached checkout；没有合主线。远端/本地证据 `logs/977026b2-core-candidate/`、`_tmp/20261113-e0-resume/977026b2-core-candidate/`，父 PID1017136 已退出。

- fmt exit1：`runtime/session/factory_tests.rs:1250` 的 assert_eq! 需要 rustfmt 合一行；首败保留并告根。
- gateway all-targets clippy exit0（1m56，含外部 cargo install nextest0.9.150 的共享缓存锁等待）；本任务使用 tools/rust/bin 内固定 nextest0.9.148，未启停外部安装进程。
- lib 精确受影响测试 9/9 通过：production adapter、扩展保留名/别名碰撞、native_component、PDF 与 read_file PDF 子集。
- integration 16 run、15 passed、1 failed，0 skipped。默认入口、Google2、公开扩展、PDF multimodal及新增valid/corrupt测试均通过。
- 旧 builtin_tool_outputs_match_real_ts 越过已修PDF之后，暴露 `time-alias`：Rust 报 `Invalid timezone: US/Pacific`，oracle预期成功。只读确认 cb8 `/usr/share/zoneinfo/US/Pacific` 缺失、`America/Los_Angeles`存在；历史提案 `A8.3-CRON-TZ-ALIAS-001`/R1 与 schedule_tests.rs:9-13 记录 split tzdata 的该类受限环境行为；根复核条目末尾明确未批准，不能视为已接受差异。已报根处理产品解析，当前仍保留失败，不放宽测试，不修改系统树。

该候选尚不满足全部通过：fmt 与 builtin tool 时区解析待闭合。四项修复的 native-process 负控尚未执行：脚本每项包含正控→实际变异编译→nonce断言杀死→源码恢复正控，会反复重编 gateway lib；按根排期后执行，不能以 `--check` 代替实际杀死证据。当前构建队列空闲。

纠正记录：先前沟通将时区条目简称为既有差异，容易误导为已批准。根已核实该条目明确“未批准，非已接受差异”；本批失败必须闭合，不放宽断言。

## 四项 native-process 负控尝试（977，尚未取得杀死证据）

根授权执行四项实际负控：configured-hooks、configured-modules、title-cancel-wait、title-timeout-wait。`negative_controls.py --check` 66个锚点及断言绑定通过，但不能代替实际杀死。

- 首轮 `logs/977026b2-native-negative`：装置在变异前拒绝 CARGO_TARGET_DIR 位于 src 内，4 INVALID、0 compiled、0 killed。
- 为满足源/构建隔离，已将本任务既有315GB `checkout/src/target` 原地移动至 `checkout/_build/negative-target`，没有复制构建树。临时兼容symlink会成为untracked源码输入，已撤销。后续本任务 Cargo 必须显式使用 `CARGO_TARGET_DIR=/home/clawbox-08/ws/pilotdeck-rs-cc2c/checkout/_build/negative-target`。
- 次轮 `logs/977026b2-native-negative-r2`：source guard 在变异前拒绝，4 INVALID、0 compiled、0 killed。`source_inputs.rs:453` 报 local_settings.rs 存在无法解析的间接include；实际该文件238行变量名 `include`，239/247行 `matches!(include, ...)` 被 `names_indirect_include` 的词法判据误认。已报根分配修复，未绕过守卫。
- 两批均已结束，最终 git status clean，未发生实际源码变异。完整日志本地同名证据目录保留；队列空闲。

TZ只读取证：cb8 `/usr/share/zoneinfo/tzdata.zi` version2025b，4577行 `L America/Los_Angeles US/Pacific`；SHA256 `a7b113737e97a9b58f9f040a1b1e7e3af6e800649b1c381b601a9c69d425dc70`。canonical America/Los_Angeles SHA256 `68977bb9ad6d186fefc6c7abd36010a66e30008dcb2d376087a41c49861e7268`。已直接交 native_regression 按根授权实现同root数据解析；没有修改系统目录。

## Core v2 固定候选 824b9160

固定 SHA `824b9160546691aa365aac6b88144cdee99adeb8`（parent977），Git bundle传送，clean detached checkout；未合root主线。远端 `logs/824b9160-core-v2/`，完整本地副本 `_tmp/20261113-e0-resume/824b9160-core-v2/`。父PID1136738已退出，最终restored-status空，源码恢复clean。

- fmt exit0。
- gateway+transport all-targets clippy exit0。
- transport `--test system_clock` 全44/44通过（含新split timezone links三项）。
- 精确lib9/9通过。
- `builtin_tools`五项、`model_complete_google`两项、`mode_routing`、`native_extensions` 合16/16通过；原PDF及US/Pacific首败均在该固定源码真实组件环境下闭合。
- 四实际负控：configured-modules、title-cancel-wait、title-timeout-wait为KILLED，当前nonce绑定断言命中；configured-hooks为SURVIVED。四项均compiled=true、restoredPass=true，0 INVALID。此批不能称全部负控通过。
- hooks存活因果由native_regression只读定位：config snapshot白名单不保留hooks，因此mutant检查config.hooks的条件恒false；未知字段在diagnostics path hooks。根待固定后续修正注入与正控因果，不放宽judge。所有首次survival日志保留。

同批正向检查均通过，不代替里程碑全workspace/跨平台或发行验收。队列现空闲，等待根新固定SHA后仅重跑hooks必要验证。

## Core v3 hooks 最小修正验证

固定 SHA `cf143ff81c2277745ccd6870e5e1151de811561e`（parent824），ref `codex/backend-resume-core-candidate-v3`。仅2个测试/负控文件改变，Git bundle传送到clean checkout；未自行合主线。远端 `logs/cf143ff8-hooks-v3/`，完整本地副本 `_tmp/20261113-e0-resume/cf143ff8-hooks-v3/`。

- fmt exit0，gateway+transport all-targets clippy exit0，66 anchors/static绑定 exit0。
- 仅重跑 configured-hooks，实际正控（包含diagnostics hooks存在及normalized config.hooks已消失因果）通过，变异编译成功，nonce绑定断言命中为KILLED，恢复正控通过。summary compiled=true/restoredPass=true，1KILLED/0SURVIVED/0INVALID，mutations exit0。
- 最终restored-status为空，源码clean，父PID1285278退出。没有重复824已KILLED的另外三项；824首次hooks SURVIVED记录未覆盖。

本次窄链的正向回归与四项实际负控缺口已闭合：正向回归属于824，hooks最小测试修正的fmt/clippy与实际负控属于cf143；不同SHA证据按各自边界引用。尚未完成同一最终集成SHA全workspace四门、正式BBX及发行验收，不能称后端整体交付。构建队列当前空闲。

## Root 集成 WIP 81bc Linux 首轮

固定 `81bc7e5d5db040247a362e1e9c723af5d5a0853e`，Git bundle后clean checkout。check gateway+CLI all-targets exit0，gateway lib --no-run exit0；未跑clippy或全workspace。首批 `logs/81bc7e5d-linux-first/` 保存编译与telemetry21/21；宏列表来自root sealed mac identity而不是手写省略失败。

H1执行前名单guard拒绝124对123：唯一缺少whatsapp bridge macOS进程budget测试，源码明确cfg(target_os=macos)，故单列Linux不适用，而非跳过失败。新目录 `logs/81bc7e5d-linux-tests-r2/` 使用明确Linux123名单继续；不重复已通过的telemetry。完整本地同名目录均在 `_tmp/20261113-e0-resume/`。

Linux共252次执行、247唯一适用测试：241通过、6失败；另外1 macOS-only不适用。分组：telemetry21/21、owned25/25、S1-hooks13/13、shared26/26、loader39/39、restore-regression5/5；H1为117/123通过。失败保留：DingTalk disconnect与WeCom reconnect为mock端ConnectionReset；Discord两项在accept_hdr处InvalidUri（并导致auth超时/expected bot缺失）；QQ与Telegram分块期望不符。完整错误含巨大字节数组，原始日志未裁剪；精简索引 `81bc-linux-h1-first-failures.json` 仅供查找不代替原始证据。mac已知7首败中HA rejected_reconnect Linux本次通过，其余6失败，未抹除mac首败。各组binary SHA与精确测试集合见identity.json。队列结束源码clean。

## SDK cancel harness TS self Docker 首败

固定harness `5046398b15861f3e7702534b5a6c826efde5c566`（独立parent cf143），run/scenario哈希与C1 E0-queue相符。只跑TS双轮自比；没有RustCLI重放。使用既有Docker image `sha256:eb58eb269804b8a9b2bb9ceeb9a175cac9b79612d4c8dafe0a987b277bd55683`，无pull，network none、非root、无宿主端口映射，保留18789 guard与隔离HOME。

结果exit1：0/1 SAME、DIFF2（不是INVALID/端口拒绝）。差异在serve.stdout与stderr；第二次基线SDK取消发生 `PilotDeckError: Gateway client closed.`（sdk/dist/transport.js:160），第一次正常输出CLI interrupted。两侧persisted seq6 assistant_text_delta触发与provider open/flushed1证据均存在。首败完整保存远端 `logs/5046398b-sdk-ts-self/` 与identity sibling，本地 `_tmp/20261113-e0-resume/` 下同名目录；已交C1validator与sdk_harness_review只读归因，不自动重跑。runner1445541退出，容器自动rm，源码clean。新RustCLI未出，禁止用旧CLI冒充当前root重放。

## SDK 双序因果诊断（非正式自比）

根派H1 matrix作者准备driver，E0核SHA `5ea3f159af414e09f05c2819ec51490ec76ecd15c9ba7224d8a06a8afb8c48c7` 后在既有Docker digest中执行；固定TS baseline另加只读mount、network none/非root/无映射，容器内临时端口，不使用18789，不改harness或TS。driver在真实CLI输出partial后发送真实SIGINT，以受控mock Gateway分别门控abort response和submit final。

执行driver exit0，两序均signaled/abortObserved/submitObserved=true、CLI code1/signal=null；abort-first `unhandledClosed=false`，final-first `unhandledClosed=true`。原帧与纳秒时间线、stdout/stderr、源码哈希已同步 `_tmp/20261113-e0-resume/5046398b-sdk-order-diagnostic/` 及identity sibling，并交H1matrix、SDK reviewer和根核因果。hostrunner1557522退出，容器自动rm。该诊断支持竞态归因，但不是正式TSself通过，不能覆盖504正式DIFF2。

SDK诊断措辞纠正：abort-first的unhandledClosed=false只表示没有未处理堆栈；其stderr仍有main.catch捕获的单行Gateway client closed，且没有正式self首轮的aborted JSON。首轮mock不能证明正式self当次的实际帧序。

SDK诊断r2：新driver SHA `92cf45cedcf979c37e6c9b601b9267f97971f9a0c5c96a9517f89c5c51e114d7`，首driver/首证据保留。三种门控driver exit0，均真实SIGINT/submit/abort观测、CLIcode1：final+abort同corked发送批次输出aborted JSON、无未处理堆栈（stderr166B）；setImmediate下一eventloop与直到client close两种分离交付均出现未处理Gateway client closed栈（stderr1511B）。完整原帧/时间线在本地 `_tmp/20261113-e0-resume/5046398b-sdk-order-diagnostic-r2/` 与identity sibling，已交作者和review。该受控mock的因果支持不能证明正式self当次帧序或代替最终真实Gateway验收。

SDK r2协审边界：cork/uncork只证明发送侧批次，未直接观测客户端TCP read边界。next-turn的abort response readyState=1且send callback成功仍触发未处理close；separated在readyState=3时已无法发出。

SDK诊断r3：driver SHA `97e2fefbf51decf454bfec348e2410b32d9a24ac0a2be12e9fd7eaec98cc4fd6`。terminal event后等待真实CLI stdout result，再corked发送submit-final+abort-response；结果stdout为aborted_streaming completed result后紧接aborted JSON，stderr仅SQLite ExperimentalWarning，无Gateway错误或未处理堆栈。driverexit0，真实SIGINT/submit/abort均观测，CLIcode1。完整原帧/身份在 `_tmp/20261113-e0-resume/5046398b-sdk-order-diagnostic-r3/` 及identity sibling；已交作者与review，正式self首败仍保留，尚不计SAME。

## 集成候选4525全 workspace Linux

固定 `4525bb2abdee1e7e1f398889d9ad56baa3337367`，clean checkout，单heavy jobs6、unset CARGO_BUILD_JOBS、umask022、真实MuPDF/image/rg、隔离HOME/PILOT_HOME。远端 `logs/4525bb2a-workspace/`，完整本地副本同名。最终父1594223退出、source clean、余盘410GB。

- 全workspace/all-targets clippy `-D warnings` exit101：完整56条带位置诊断索引保留 `4525-clippy-diagnostics.json`，不只摘最后一条；lib46项及libtest额外诊断，未当作通过。
- CLI/gateway debug build exit0。CLI SHA `73136b3e676a5145309dcc0e4c90038f5e2ece611470d0f77de2f5b3f131ff6f`；gateway SHA `4aef4ef47d4c66794e2429bfc86c871266d9a6ee947337afef41605516bda0e8`。不能当release。
- workspace nextest exit100，651.416s：8241 run，8238 passed（8 slow），3 failed，742 skipped。
- 三失败：native_ws::tests::legacy_proxy_body_unchanged 的#749源码hash guard（实际082782f5…，期望02613cf4…）；owned::channel_bridge_preserves_optional_fields_for_both_submission_modes、owned::legacy_channel_submit_starts_before_its_future_is_polled 均submit_owned_tests.rs297 5s owned lifecycle settle超时。首次日志及时交根，未修改source或断言。
- workspace doc tests exit0。

## 组合场景首败与独立419 harness

4525原组合TSself首轮两侧唯一capture failures为phase-2-disconnect（INVALID），strict verify均因no hidden capture failures失败；6corruption正控前置不成立，没计通过。原capture/identity与 `4525bb2a-core-channel-verify-first` 完整保留。

修正候选 `419773804a4bdaa7b87c7efbb6ed470d2bdf618b` 独立clone在taskroot/harness/41977380-core，未切heavy4525源码。scenario SHA1fc400370e3e7825867b78774ef94ce7fe95acde4ebe610ac61c01a078400606；verify b0ee912c318a651135043d336e873a0b2a3d96ce96d8ebabc2103b0d8e5dbbd5。

419 TS双轮capture均无failures，但self exit1/DIFF7：仅七个channel-poll URL的after epoch秒差异（1794523716/3725 对1794523731/3740）；未新增归一化。严格门两侧又暴露私有verify.mjs57：actual是真newline，expected为String.raw literal backslash-n。完整首败 `41977380-core-channel-self/` 与 `41977380-core-extra/`（含identity）已本地同步并交inventory/root。6corruption及actualwrongreply受positive前置失败未执行，不能计通过；保持原scores/原capture。

## D1 Linux只读准备核验与后续顺序

未跑旧测试、未修改checker。既有Linux aarch64组件身份仍一致：MuPDF helper bf370bfe…，image helper1e76e87d…，libvips-cpp0be6a13b…；readelf确认三者最高GLIBC_2.38，image RUNPATH为`$ORIGIN/../lib`，源码及许可目录齐备。源归档MuPDF SHA `31d856aff4a7bbbb3d94d1acff78a4c7e835169ddf8747a2c6ca782c150b9630`，vips SHA `8bc6b8318aa7b5f8e354af75e8b0af336f3b08036eab568f7fb41e92b9c4b904`。此前组件测试可作各自身份的历史证据，正式组包仍核完整文件清单/动态依赖。

当前无release目录、正式发行包或最终package manifest。D1本机277包许可归集通过只属于其候选/cache；最终Linux须从同clean集成SHA生成原始locked/all-features metadata并在原cache重归集。内建技能/插件资源仍pending；macOS组件正在源准备，尚无编译/发行证据；x86_64无本轮发行证据。

最终候选必需顺序：固定完整集成SHA并闭合适用全量门→同SHA release构建CLI/独立sidecar（gateway若纳包须同构建）并归档命令/退出码/target/toolchain/hash→完整metadata及锁依赖许可归集→record.py→core/full assemble（full使用上述组件及source/licenses）→verify→搬迁smoke→仅挂载包、不挂TS参考仓的隔离正式smoke→资源/平台/性能及正式组合重启验收。不能用debug二进制、目录存在或工具装置测试替代这些门。

## AGENT03 SDK fork 首校准

独立harness候选 `643493af4c667e52b845c24579716b1c8bd7b4b0`（parent4525，仅3fixture文件），taskroot/harness/643493af-agent03 clean固定。先单次 `run.mjs ts .../scenario.json capture.json`，没有先假设成功跑self。既有Docker digest、network none、非root、无端口映射、保留18789 guard、隔离HOME，未修改TS或产品。

runner1749765已结束，run.mjs exit0但artifact含unexpected `steps[0] timeout`，不能记业务成功。实际provider title2、parent1、child-alpha1、child-beta1、child-mcp1、child-after-mcp1、second1；缺observer、parent-after、parent-check。完整原artifact与source/命令/image/docker日志本地 `_tmp/20261113-e0-resume/643493af-agent03-first/`，已交S1review与根校准首因。初版private verify未阻断采集；因首轮未实际成功，尚未进入TSself或strictnegative，不放宽断言。队列空闲。

AGENT03第二候选 `e47c5e3bec3802b6054ec39854f2ee7345bcf134` 独立harness固定后，单capture自然完成且failures=[]，精确MCP allow已通。private verifier首败child-alpha请求数2≠1；child四阶段均2、observer1/parent-check1/second1、缺parent-after。随后按实际capture成功继续固定TSself，exit1/DIFF217，起始resultBytes669/668、context/token链等；完整原两capture+diff本地 `_tmp/20261113-e0-resume/e47c5e3b-agent03-self/self/`，单首capture与verify首错在同级e47c5e3b-agent03-first。

S1review只读定位parent-after规则落在child规则之后，父工具结果含child directive导致误命中并重复child链；observer_report字面期待也不符真实WS投影。等待根固定后续校准，未调整volatile，private校准错误不当产品缺陷；strictnegative有效正控尚不成立，未执行/不计通过。两runner均退出，队列空闲。

## 0e统一正式harness的组合与AGENT03

固定 `0e214d726bdec2758c55786819af65b7c6ec9c72`，独立harness，未改heavy4525源码。两个轻槽运行结束。

组合：TSself 1/1 SAME，strict A/B通过；六项artifact破坏负控全被拒绝，原正控hash保持。实际wrongreply派生scene真实两次server capture成功，strict因phase0历史不含应有CORE-DONE而非零拒绝（真实模型错误穿过业务链），不把JSON破坏替代实际负控。完整证据 `_tmp/20261113-e0-resume/0e214d72-core-{self,extra,extra-run}/`，正控artifact SHA80e7be615a36e2bf60f9b7266257cf3f73b674d752fa9c857417f57802ff3961。

AGENT03：第三单capture及其private strict七项业务检查通过；TSself仍DIFF90，context/token链等，private strict两侧均通过但不能代替self。全证据 `0e214d72-agent03-{first,self}/`；已交S1只读原因，不重跑，不加忽略。actual MCP错回复负控尚未执行，根后续优先slash事实核验。

## 插件slash正式WS行为事实

在0e固定harness与TS基线，原slash-plugin-command单capture failures=[]，原短名self1/1SAME。六项未注册短名/unknown/leading空格原样进入model user与durable accepted_input；这个事实仅覆盖这些输入，旧scene about对所有plugin不展开的推断不能成立。

按根指示只派生四个message为全名：/cmd-plugin:hello（带args/空）及/cmd-plugin:empty（带args/空）；其余steps/provider/files/unknown与leading空格对照保持。派生scenario SHA `c952aaf2029d26020076c1bb23dc6dd873444eda8bc993258a27c8696e426ff1`，完整before/after manifest在 `0e214d72-slash-plugin-qualified-input/`。

真实qualified capture failures=[]：hello已展开为plugin-command包装，body保留`Say hello to $ARGUMENTS.`字面，参数单独command-argument块；带参碰撞marker使body分隔符编号1/argument0。无args仍有body但无argument块。空body命令为 `Run plugin command "/cmd-plugin:empty" with argument: some argument` 或 `Run plugin command "/cmd-plugin:empty".`。model最后user与落盘accepted_input文本完全对应。完整原artifact在 `0e214d72-slash-plugin-qualified-first/`，事实索引 `slash-plugin-facts.json` 保留原artifactsha、model、accepted及commands_list原始帧；已直接交root/Protocol/O1。限定全名vs短名，不泛化为所有入口。

qualified派生场景TSself也已完成1/1SAME（exit0），证据 `0e214d72-slash-plugin-qualified-self/self/`；原短名对照未改。两类实际行为在同固定基线上各自自比稳定。当前E0重型/轻量均空闲。
