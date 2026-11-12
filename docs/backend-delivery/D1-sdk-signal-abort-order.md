# D1 SDK signal abort 生命周期

任务负责人 h1_matrix_discord；固定基线 cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85。范围独占 CLI sdk/{turn,output}、新 sdk-signal-abort-order fixture、独立差异 D1-SDK-SIGNAL-ABORT-ORDER-001。不修改共享正式 harness、SDK显式cancel或旧非signal差异。

状态：实现后待验证，尚未自行 Cargo。封存 4525 binary e408a2bf557d106e0043134ecf2fe7bca0b3602535451a2ff28b67eb8ce6d06b 首次随机WS三门控全部真实SIGINT/submit/abort、自然exit1，但stderr Gateway client closed、canonicalAbort=false；首次证据 `_tmp/20261113-sdk-cancel-order-diagnostic/rust-4525-first/` 未覆盖。

修复最小化：单真实OS signal和单当前query/run abortRPC，stream-final先到保留transport等该RPC，仍使用既有request timeout；成功ack才设TextOutput::Aborted并输出准确JSON/exit1。provider事件投影不变，不重发RPC、不新增submit。错误/无关响应/断线/超时不变成功，非signal路径及双signal旧语义不扩修。

8项独立driver/严格verify已写：batch/next-turn/held三个正门、无关run RPC ID、malformed、RPCerror、连接drop、既有10s timeout。需要根集中编译新正式binary后执行，不能把目前语法/rustfmt结果称测试通过。严格verifier要求实际signal/精确currentrun/session/RPC ID/精确JSON和stderr/两socket释放/binary身份；不改旧比较器或formal延时。

TS原正式自比DIFF2、三批因果诊断的原始证据与结论完整保留，详见 `_tmp/20261113-sdk-cancel-order-diagnostic/result.md`。Gate诊断是真实固定CLI+精确协议mock，不包含真实gateway/provider/持久化；真实持久resume由E0新候选formal单独验证。

五层：接口待验证、实现待验证、接线既有正式SDK CLI路径待验证、协议门控待新binary、发行及持久恢复待验证。未推送、发布或使用真实凭据。

给根审核的正式比较策略：原TS双自比先原样记录baseline unstable；仅命中本条真实signal/current-run pending abort的具名c1-sdk-cancel-resume serve step，由独立正判要求Rust canonical JSON/exit1/无stderr/真实abort请求及响应/两连接释放，同时严格保留原provider终端与持久恢复/历史/模型请求断言。其余步骤和原差异比较不变；误删partial、错run、漏RPC、伪造无signal、错误abort被当成功、resume失败均必须失败。不得把不稳定TS stdout/stderr整体移入volatile/accepted或放宽其他CLI输出。此策略尚未修改比较器，待根审核与新binary证据。
