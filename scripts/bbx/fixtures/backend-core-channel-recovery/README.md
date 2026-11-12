# 核心渠道→工具互动→结束→重启恢复组合

正式入口是两个按序启动和退出的真实 CLI `pilotdeck server`，使用默认native配置；同一隔离 HOME/PILOT_HOME、真实 BlueBubbles adapter、真实Gateway、Agent loop、文件工具、权限协调器和会话持久化。测试只模拟外部平台HTTP和模型HTTP，不import产品、不注入MockTurnGateway、不安装SDK、不使用真实账号。

本目录的 `verify.mjs` 是本组合的**必需额外门**，不修改公共run/evidence。单独 `SAME` 不能代替它通过。新增JSON已使用现有内建assertions核对两次启动/退出、模型请求唯一性和实际文件内容；独立门进一步核对完整历史来源、渠道权限提示、同一durable session和零重复副作用。

## 单次场景如何工作

- 原harness模型mock已有 `method` / `path` / `reply.status` / `reply.body` / `save`。用GET `/api/v1/message`模拟BB轮询，用POST `/api/v1/message/text`记录真实渠道出站，其余POST `/v1/chat/completions`是真实模型请求。
- 初始BB行 `guid=core-message`、文本 `CORE-BEGIN-{saved.channelText}`。未赋值的placeholder是明确固定输入，不是被测代码。第二行的不同guid为 `{saved.answerGuid}`，文本 `1`。首turn真实write_file产生权限提示并等待，下一次实际重叠轮询交回同chat用户答复，不能由WS代答。
- 真正写工具完成后的模型请求必须包含 `call_core_write` 的工具结果。只有该真实请求命中after-tool规则，观察其中工具arguments的 `CORE-RECOVER` / `core-message`，由现有save改写后续mock输入。文件实际内容为 `CORE-RECOVER core-message\n`，位于隔离PILOT_HOME/notes，避免对真实主机路径的权限。
- 新进程仍同HOME、同chat general。首行变为 `CORE-BEGIN-CORE-RECOVER`，答案行guid已变为core-message，与本次首行相同，所以被真实去重，不制造第三个turn。恢复模型请求必须含第一轮唯一user、唯一write_file调用及原始tool结果、CORE-DONE助手历史；仅追加一个恢复user。末尾真实渠道回复CORE-RECOVERED。
- 两次固定有界等待后都用真实WS `read_session_messages`读回执，随后正常close客户端并SIGTERM精确server PID。任何尚未完成都由请求/历史/文件断言失败，不能用sleep成功替代业务成功。

现有persisted-run-event trigger只适用于正在运行且provider仍open的partial事件，本场景选择自然完成和落盘，所以不能拿该trigger冒充完成通知；保留8s/5s有界等待，读回执严格失败。若self暴露慢机同步问题应新增正式可验证触发能力，不能放宽断言或无限睡眠。

## 执行环境和身份

本机18789存在非本任务listener，Docker daemon不可用。**本机不运行本场景**，也不关闭listener；harness全局18789 guard保持。交E0在cb8既有隔离容器/私网执行，非特权UID，不映射宿主端口。不要求产品监听18789，server与模型使用harness槽位3841x，并由BBX_SLOT替换。

先冻结公共harness、完整工作树/新场景与三个fixture文件，再在固定源码checkout中运行。inputIdentity包含根Git dirty/untracked与harness hash，不能在根持续写Rust时拿同目录跑TS而声称source不变。固定TS源码与现存dist/依赖也必须由E0身份校验；Rust等新binary及对应源码SHA，禁止BBX_ALLOW_STALE=1。

```sh
# 这些是隔离cb8环境内命令，不在裸宿主执行；按E0实际source/Node路径替换。
node scripts/bbx/check.mjs --self backend-core-channel-recovery --out NEW_SELF_DIR
node scripts/bbx/fixtures/backend-core-channel-recovery/verify.mjs --self-test ACTUAL_TS_A_JSON
node scripts/bbx/fixtures/backend-core-channel-recovery/verify.mjs ACTUAL_TS_B_JSON
# 新Rust binary可用后，同一scenario/harness/source快照：
node scripts/bbx/check.mjs backend-core-channel-recovery --out NEW_PARITY_DIR
node scripts/bbx/fixtures/backend-core-channel-recovery/verify.mjs ACTUAL_RUST_JSON
```

当前没有增加任何volatile/accepted。GET轮询次数和after参数可能在TS双轮变化，先保存实际self差异；只可根据真实双轮证据后增窄规则，并保留独立门对每个原始GET的method/path/password/limit/after及阶段模型请求验证。不能整体忽略渠道出站、历史、provider模型请求或落盘。

## 必需负控

`verify.mjs --self-test actual-valid.json`先验真实正控，再对原始artifact的副本做6种破坏：漏恢复请求、重复首轮副作用、丢第一轮助手历史、错文件字节、无权限提示、错渠道恢复回复。每项必须拒绝，原始artifact哈希前后不变。这个分母是装置负控，不计产品通过。

真实正式链负控：

```sh
node scripts/bbx/fixtures/backend-core-channel-recovery/negative-scenario.mjs NEW_NEGATIVE_SCENARIO.json
node scripts/bbx/run.mjs ts NEW_NEGATIVE_SCENARIO.json NEW_NEGATIVE_ARTIFACT.json
node scripts/bbx/fixtures/backend-core-channel-recovery/verify.mjs NEW_NEGATIVE_ARTIFACT.json NEW_NEGATIVE_SCENARIO.json
```

负控只把本地模型after-tool应答改成CORE-WRONG-REPLY；最后一条必须非零，并保留真实两server进程、请求、文件、错误历史和source identity。它是实际模型mock错误穿过正式业务链，不能描述成Rust产品源码mutation。没有执行过的负控不算通过。

## 当前状态

JSON与两脚本语法已检查；TS双轮、真实错误负控、独立6格negative、Rust正式重放均未执行。未改run/evidence，未改产品源码，未使用真实凭据/账号/18789服务。
