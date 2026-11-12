# H1 WhatsApp 恢复候选

日期：2026-11-13。起点C1 `bcad7cb1435103e31c9ec71877c7064f3cffc619` 加停工WIP；未提交。负责人 h1_inventory。固定基线 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。本轮授权独占whatsapp目录、其fixtures、本页及三项新窄差异；公共mod、loader、mount、network、assembly由主协调修改。StaffDeck延期。

## 正式入口及状态

正式入口为隔离HOME/PILOT_HOME的native `pilotdeck server` + `adapters.whatsapp.enabled`和extra.bridgePath/bridgeUrl，启动用户外部Node bridge，经本地mock HTTP与真实模型回合。核心默认不依赖Node。既有C1未挂9文件与20项测试恢复为候选；本轮另增2项普通start因果测试，共22项。接口：已有明确配置；实现：进行中；接线：主协调进行；验证：待验证（尚未Cargo）；发行交付：待验证。不能用本页或窄差异条目存在替代通过。

## 本轮改动及依据

TS `src/adapters/channel/whatsapp/WhatsAppChannel.ts:70-108` 同步spawn参数校验throw进入catch，记录start failed并返回noop；常见OS spawn ENOENT通过child error异步触发，仅记录bridge spawn error，之后readiness照常执行。这两种错误不应混为直接start Err，也不能统一标start failed。

`bridge.rs`新增内部SpawnFailure区分同步和child错误。NUL bridgePath复用现有node_io Node格式器；同步错误经过cleanup返回noop；OS错误日志使用既有errno映射，仍运行readiness。pipe setup错误归同步清理，不能遗失已经启动的child owner。资源清理失败仍传播真实错误，避免伪装成功。`mod.rs`依据错误种类处理，原readiness失败/成功路径保留。

新增 `start-oracle.mjs`实际调用固定TS adapter，不启动真实服务。前三格NUL参数在native spawn创建child前同步throw，要求readiness0、child null、running false、单条精确TypeError日志及可调用noop stop。两轮 `_tmp/20261113-whatsapp-start-{a,b}` 实际通过，oracle字节相同SHA256 `814a750fc4813e1187f49e5fc6a9e0eeed167694f8557bda33929d15c9fb211d`；后续第四格真实OS spawn/注入executable因果驱动在 `_tmp/20261113-whatsapp-start-{c,d}` 两轮成功，新四格oracle字节相同SHA256 `fa12beaaf6bf4875b25d5713b9c7d8f8aa8bed269f1b7ae3f4174972593cf686`。`start-oracle.json`和`start-metadata.json`现来自c轮；原a/b证据保留。第4格只将执行文件改为不存在路径，仍由真实Node spawn发异步ENOENT，readiness独立注入true，确认仍connected/running。不存在子PID，清理不制造虚假进程通过。

新Rust `synchronous_start_failure_matches_real_ts_and_never_polls_http`逐条消费上述三格原始日志并断言HTTP mock调用0和无child；`os_spawn_failure_emits_child_error_and_still_checks_readiness`单独使用不存在executable，HTTP mock成功，断言异步错误日志后仍running且stop可关闭。

原tests/process_tests用了Fetch禁止端口1，会在HTTP mock之前失败，重复C1 Signal已实际定位的夹具问题。本轮只将自有夹具URL及对应精确URI断言改18081，没有放宽错误断言或改产品网络语义。

## 已知bug差异

追加 `H1-WHATSAPP-STDOUT-001`、`H1-WHATSAPP-DESCENDANTS-001`、`H1-WHATSAPP-STOP-DRAIN-001`，分别为stdout挂起、Unix孙进程泄漏、禁用后活动turn资源残留。引用用户执行计划授权与本会话主协调明确派工，范围均窄于普通协议。原TS证据位于 `_tmp/20261113-h1-next-matrix-whatsapp/{bridge-stress-a,bridge-stress-b,final-a,final-b}`；本轮已重读，不声称本轮重跑这些正式场景。

## 验证与下一步

已执行：真实TS同步三格a/b两轮、完整四格c/d两轮；直接rustfmt自有Rust文件通过。未执行：Cargo check、clippy、22项Rust、正式native五场景、三差异Rust正向断言。构建由validation集中排程，本作者未自行Cargo。下一步由validation先all-targets check及22项；保留首失败，修复后受影响重验。正式对拍需固定集成SHA/二进制/场景hash；旧20项和TS通过均不能代替当前native五场景。

Unix process_group/waitid NOWAIT需Linux/macOS实测；Windows阻塞pipe读取与孙进程释放无同等保证，跨平台发行需明确处置。stderr按块UTF8解码待oracle核验，不擅自当bug改写。没有改变正常HTTP协议、配置字段或轮询间隔；未调用旧TS Gateway/Agent作为产品回退。


首编失败保留：validation `_build/20261113-052224-resume-second-lib-test-compile/output.log` exit101，WhatsApp `tests.rs:369` E0599（JsObject没有iter）。确认编译退出、受测源前后未变并由主协调明确释放后，只将该循环改为已有 `own_entries()`，未改断言或产品。其它包错误归各作者。本轮修复尚未重编，不能记通过。


## macOS 首跑EPERM定位及窄修

首次22项实际结果20 passed / 2 failed / 0 ignored，`_build/20261113-052745-resume-third-wa/output.log`，二进制SHA256 `e42f7775c296488eed700b61c96c502f1222e3b26bad1deb43387ab20cecf931`。失败为stdout排空及start等待者drop的两项进程测试，均最终组信号报EPERM；其余18普通+2进程通过。原失败保留。

只操作探针所属PID/PGID的macOS实际因果材料在根工作树 `_tmp/20261113-whatsapp-mac-pgid/`。c/d及e/f握手后均证明：unreaped zombie-only组killpg(KILL/0)返回EPERM，waitid NOWAIT已见exit且精确PID状态Z；仍有live leader或忽略TERM的live descendant则killpg成功。a轮孙进程handler尚未ready有竞态，不用它作为live descendant证据；修正探针后的原始结果保留。e/f实际 `/bin/ps -g <owned pgid> -o pid=,pgid=,stat=`只列该组，分别得到singleton Z、singleton S和Z+S两行。

不扩大既有unsafe边界：未修改child-signals、unsafe_boundary或Cargo。仅macOS遇EPERM时，先用已有waitid NOWAIT确认本实例unreaped leader已退出，再执行绝对/bin/ps精确组查询；只在完整成功输出恰一行同PID/PGID且Z状态时覆盖本次EPERM，随后reap。额外成员、活进程、未知/错误/截断/非UTF8/空输出均保留原错误。查询env_clear+LC_ALL=C，无shell/PATH依赖，stdout非阻塞、8192字节上限、2秒预算，RAII在所有路径kill/wait精确查询child；原leader始终未reap以保留所有权。

新增singleton proof负控一项（额外/live/unknown/截断/失败等），macOS真实查询预算及子进程释放一项（timeout和过大输出两格）。现macOS分母24项、其他Unix23项。直接rustfmt通过；这次修改尚未集中重编或运行，原20/22不能覆盖新候选。下一批需首编及完整24项，特别保留全部四个真实bridge进程回归；不skip、不放宽任意EPERM。
