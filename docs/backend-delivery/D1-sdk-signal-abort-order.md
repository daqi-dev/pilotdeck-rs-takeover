# D1 SDK signal abort 生命周期

任务负责人 h1_matrix_discord；固定基线 cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85。范围独占 CLI sdk/{turn,output}、新 sdk-signal-abort-order fixture、独立差异 D1-SDK-SIGNAL-ABORT-ORDER-001。不修改共享正式 harness、SDK显式cancel或旧非signal差异。

状态：实现后待验证，尚未自行 Cargo。封存 4525 binary e408a2bf557d106e0043134ecf2fe7bca0b3602535451a2ff28b67eb8ce6d06b 首次随机WS三门控全部真实SIGINT/submit/abort、自然exit1，但stderr Gateway client closed、canonicalAbort=false；首次证据 `_tmp/20261113-sdk-cancel-order-diagnostic/rust-4525-first/` 未覆盖。

修复最小化：单真实OS signal和单当前query/run abortRPC，stream-final先到保留transport等该RPC，仍使用既有request timeout；成功ack才设TextOutput::Aborted并输出准确JSON/exit1。provider事件投影不变，不重发RPC、不新增submit。错误/无关响应/断线/超时不变成功，非signal路径及双signal旧语义不扩修。

8项独立driver/严格verify已写：batch/next-turn/held三个正门、无关run RPC ID、malformed、RPCerror、连接drop、既有10s timeout。需要根集中编译新正式binary后执行，不能把目前语法/rustfmt结果称测试通过。严格verifier要求实际signal/精确currentrun/session/RPC ID/精确JSON和stderr/两socket释放/binary身份；不改旧比较器或formal延时。

TS原正式自比DIFF2、三批因果诊断的原始证据与结论完整保留，详见 `_tmp/20261113-sdk-cancel-order-diagnostic/result.md`。Gate诊断是真实固定CLI+精确协议mock，不包含真实gateway/provider/持久化；真实持久resume由E0新候选formal单独验证。

五层：接口待验证、实现待验证、接线既有正式SDK CLI路径待验证、协议门控待新binary、发行及持久恢复待验证。未推送、发布或使用真实凭据。

给根审核的正式比较策略：原TS双自比先原样记录baseline unstable；仅命中本条真实signal/current-run pending abort的具名c1-sdk-cancel-resume serve step，分两层独立正判：正式artifact要求Rust canonical JSON/exit1/无stderr、原始registry触发重放、真实SIGINT及PID/时间、同session/project/request/run partial与provider-open、最终三run状态及历史/模型恢复无重复；正式artifact没有abortRPC原帧，不声称在此层观察到请求/响应。真实abort请求/匹配响应与两连接释放由同binary SHA的独立8wire严格PASS收据证明；缺任一层不得接受。其余步骤和原差异比较不变；误删partial、错run、漏RPC、伪造无signal、错误abort被当成功、resume失败均必须失败。不得把不稳定TS stdout/stderr整体移入volatile/accepted或放宽其他CLI输出。此策略尚未修改比较器，待根审核与新binary证据。

私有具名逐值policy已写 `scripts/bbx/sdk-signal-policy.mjs`，API为sdkSignalAcceptedPaths({left,right,rawLeft,rawRight,scenario,selfCompare,wireReceipt})，只返回steps[1].serve.stdout/stderr两条Map路径；selfCompare永不接受。wireReceipt必须来自根独立执行严格8wire verifier的真实PASS输出，并绑定同一formal binary SHA/精确8场景；模块不把结构化收据当正式RPC原帧。公共diff/evidence/run未修改，接线仍由根审核。TS错误仅接受固定已实测完整stack SHA，未经取证的其他路径/Node版本栈不会自动放行。

## 2b3568bc 封存 CLI：完整 8-wire 实际结果

根提供正式封存 `_tmp/20261113-c1-resume-validation/root-twelfth-pilotdeck`，实际 SHA-256 `dcf56515fa38d4b3cababbdbb204a7f47e739544fb69a5ebe943ad202dfe5ae6`，绑定产品 `2b3568bc`。隔离 HOME/PILOT_HOME，WS只绑定127.0.0.1临时端口，没有接触宿主18789。

首轮失败完整保留 `_tmp/20261113-sdk-signal-policy/rust-2b3568bc-wire-first/`。前三正序/错run均已 canonicalAborted，错误控制均保持原错；失败出在装置：将真实既有30秒request deadline误认为10秒，用12秒safety提前杀了timeout case；child退出后立即terminate mock sockets、只await内部HTTP server.close，可能在WS自身close回调记录前写证据，strict因只有1条close拒绝。没有把这轮写成产品通过。

根明确授权修装置；因源码manifest冻结，只在 `_tmp/20261113-sdk-signal-policy/driver-r2/` 保存修正版，未修改冻结src fixture。修正仅timeout case safety40秒（产品30秒deadline不变），以及显式等待每个WS自身自然close promise；1秒收尾上限，超时记录diagnostic_socket_close_timeout/forced_socket_cleanup，strict新增明确拒绝，仍要求两条自然close，不能强清理后洗绿。

第二轮 `_tmp/20261113-sdk-signal-policy/rust-2b3568bc-wire-second/` driver exit0、strict verify exit0，完整8/8通过：batch/next-turn/held三正门和无关run/RPC ID门均精确abortedJSON、自然exit1、空stderr；malformed/RPCerror/连接drop/30秒deadline分别保留精确错误且没有aborted成功。每格均两个自然WS close、零强清理、零socket close超时。独立收据 `rust-2b3568bc-wire-second-verifier.stdout` 正好status/binary/cases三字段，可供根在同binary正式artifact层绑定。

实跑driver SHA `6b29ef61cf58a1403dbfc15e55f057c65ee8b1ef0afa3272f48a7a41d2d8926a`；strict verifier SHA `e4e2d422e7b96d2b67219fec81510edb329e0566b308752d95add440ef1edc87`。在独立复制的产物中加入合法seq/时间的forced_socket_cleanup负控，strict准确exit1并报告forced/timed-out cleanup不能证明自然释放，原PASS目录未动。摘要 `wire-result.json`。

现状态：该固定binary的SDK取消协议层已实际通过；正式gateway/provider/持久resume仍待E0。实跑fixture修正版等待根解冻后按同字节迁回，届时验证文件SHA，无需改Rust或重编。没有剩余产品或Node进程，轻量槽2已归还。

## Fixture 迁回及 Linux handoff

根解除src冻结后，实跑 `_tmp/driver-r2/{driver,verify}.mjs` 已按同字节迁回 `tests/fixtures/sdk-signal-abort-order/`，再次核6b29ef61.../e4e2d422...完整SHA一致；Node语法和diff-check通过，没有Rust修改或Cargo。新增该私有fixture的README记录可移植命令、绝对路径/新目录约束、实际30秒deadline、自然close正判、Linux自身binary/Node/ws/source manifest要求。

Linux仍待E0队列稳定、轻槽授权后对其实际封存Linux binary另跑完整8wire。macOS binary/receipt不能代替Linux；正式具名policy必须匹配Linux artifact输入binary SHA与对应Linux receipt。未启动额外任务或抢占E0 heavy。

## 公共比较/证据接线

根将diff/evidence SDK signal窄块及相关测试委派后，已加实际receipt revalidation与inputIdentity绑定；使用方式见 `scripts/bbx/sdk-signal-policy.md`。没有修改run.mjs或S1私有policy。原check测试临时copy新增依赖表漏新policy导致的10项首失败保留，补相关测试copy依赖后91/91通过。

具名scene实际加sdkSignalAbortOrder accepted及Rust-only footer/stderr已执行正断言，原signal/timeout/turn/存储等待保持原字节值。非self的裸diff也核对固定scene声明，双方同时删声明将INVALID；TSself不要求新声明且保持原DIFF。新增相关反例正在复核。最终接线测试只证明装置；实际Linux正式恢复未执行不能标完成。

最终本轮公共接线97/97 Node测试通过，日志 `_tmp/20261113-sdk-signal-policy/public-tests-fifth.log`；最终真实wire重验加已执行断言的synthetic formal全链在 `public-real-wire-synthetic-formal-asserted/summary.json`。其正向/缺partial/无关tree/缺receipt/原TSself退出码为0/2/1/2/1。原91测试之前的失败和各轮日志保留。所有ready文件SHA汇总在 `public-ready-manifest.json` 与 `public-ready.sha256`，实际Linux恢复仍待E0；本轮无Rust修改/Cargo或残留进程。
