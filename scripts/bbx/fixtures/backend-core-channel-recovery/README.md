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

没有整字段volatile正则或accepted行为豁免。419真实TS双轮业务capture均无failures，self DIFF7仅7个channel-poll URL中的after epoch秒数。固定TS start设置floor(Date.now()/1000)-5；本mock dateCreated恒1/2，不推进cursor、不按after过滤，因此该数值只锚定此phase启动墙钟。新声明volatilePolicies:[{policy:"coreChannelPollAfter"}]由独立channel-poll-policy严格value-check：仅精确场景/provider/GET/path，query仅password/after/limit且唯一，两个实际serve阶段的完整poll与durable accepted input时间相互核对后，只处理after十进制value；其余URL字节、headers/body、record index/count、出站、模型、历史及文件全部保留比较。公共policy/diff/evidence/run由主协调与h1_protocol实现，本作者不修改。

419 a/b两phase各4/3次poll，after a=1794523716/1794523725，b=1794523731/1794523740；各phase accepted_input.createdAt减(after+5)分别.706/.346/.899/.522秒。它们是实际观测，不把经验2秒当基线保证，policy的有界时序按场景已声明预算处理。新policy尚待固定candidate正式重跑，不能回写419原DIFF7为SAME。

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


## 首次实际self失败及精确修正

E0固定4525候选Docker TS双轮为INVALID，双方仅phase-2-disconnect断言失败，原始 `_tmp/20261113-e0-resume/4525bb2a-core-channel-self/`完整保留，不计通过。两个server自然exit0，第二阶段模型恢复规则实际命中，但第二次历史read未发到新进程：run.disconnect后ws对象仍非null且CLOSED，read步骤只对null ws自动connect，因而读取旧连接遗留__closed。场景现第二serve在history read前显式connect:true；phase2断言路径then/2→then/3，值仍严格true；私有verify按步骤类型查唯一history/disconnect并核新hello_ok在第二次read之前。没有改run/evidence、没有加等待或减弱业务断言。修正后尚待新snapshot完整TSself/严格门/真实wrongreply；首次INVALID不能用旧业务断言局部过来覆盖。


首轮raw同时核实模型initial1/aftertool1/recovered1、渠道send4以及同session两条accepted_input。scenario工具content声明为literal反斜杠n两个字符，内建file-byte断言与实际文件一致；独立verify初稿误把同一常量写成JS newline，现用String.raw精确匹配既有输入，未改场景、文件期望或原artifact。此修正待新snapshot正式正/负验证，不能据只读核对把旧INVALID改成通过。


419 strict门首次失败已保留：原String.raw“修正”误读了JSON展示转义。实际scenario、4525/419 raw文件、JSON.parse后的工具arguments均26字节且最后一字节10（LF）；不是literal反斜杠n。现在仅恢复verify两处真实LF常量，保持场景输入与内建精确file断言，不改变任何原artifact。先前文字里literal反斜杠n的陈述撤回，以本段字节核实为准。strict门正控未过之前，6破坏负控和实际wrongreply均不计通过。


恢复正确LF常量后，用verifier c073541d对419原始产物做只读独立重验：TS1正控+6corruption通过、TS2正控通过，原artifact SHA90f46687.../292b0180...前后不变，日志_tmp/20261113-backend-core-channel-recovery/strict-byte-recheck。该重验不改原SELF分数或strict首次失败，实际wrongreply和新policy self仍须由E0在新固定candidate执行。
