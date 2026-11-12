# SDK config and file events — formal BBX candidate

本候选只增加 `scripts/bbx/scenarios/sdk-config-file-events.json` 与本私有目录，不修改公共run/evidence/归一化，不import任一产品，不以Rust函数或纯bus单测代替正式入口。

当前状态：场景与严格verifier已写，JSON/Node语法检查通过；**尚未执行TS正式BBX校准或Rust比较**。预测的payload依据固定`cc7c4e6`的SessionInteractionBundle真实HTTP hook probe及createLocalGateway/settings源码，但预测仍必须由正式TS进程证实。本机18789已被其他任务占用，不启动本机BBX、不绕守卫；交E0 Docker私有网络执行，不映射宿主端口、不挂载真实HOME。

## 正式路径与断言

场景使用既有默认真实server、WS `submit_turn.sdkSessionConfig`、HTTP模型与hook本地mock。provider规则精确限制method/path，不伪造gateway返回：

1. A session配置`ConfigChange`及`FileChanged`，URL带旧event/matcher查询值，header为owner A。实际模型调用write_file创建sdk-a.txt，再由实际tool result进入模型续写。
2. B session只有自身FileChanged callback，owner B，精确matcher为sdk-b.txt；实际模型写该文件并续写。
3. 第一次真实update_settings将agent.maxOutputTokens从默认8192改为4096。必须产生A唯一ConfigChange callback，changedPaths为agent.maxOutputTokens，changeClasses为next-request；B不得继承A的callback。
4. 实际close_session A，再update_settings为2048，A旧callback必须注销。相同A key通过显式空SDK config重建，实际写sdk-reopened.txt；不会沿用旧callback。
5. B下一轮仍带自身SDK config，但实际写sdk-unmatched.txt，不应匹配sdk-b.txt。第三次update_settings为1024仍不得恢复已关闭A的callback。
6. 两个真实read_session_messages都须保留各自两轮历史；四个真实文件字节及四条durable accepted_input逐项核对。

预期HTTP hook总数是3：A文件1、A首配置变更1、B文件1。verifier逐字段核验精确URL（stale值被替换、保留retained）、header、sessionId、空transcriptPath、cwd、filePath、absolutePath、root、created及ConfigChange的精确payload；不只检查“收到hook”。不同会话、注销后回调、错误matcher、重复发送或漏发送都会失败。

场景内assertions已包含必需callback数量、字段、模型各一次+各一次续写、四文件；独立verify进一步校验所有steps实际完成、WS无error且terminal/assistant续写存在、真实settings变更、close次序、三callback精确集合、history/durable输入、运行身份、scenario SHA、所有必需assertion真实执行与正常停机。默认provider命中、隐藏capture failure或过期scenario都拒绝。

matcher使用固定TS已实跑证实的**exact相对路径**，不是glob。FileChanged body字段使用`filePath / absolutePath / root / changeType`；ConfigChange使用`changedPaths / changeClasses`。不用自创SDK协议字段。

## E0校准顺序

在E0选定的新Docker私有网络与隔离任务目录中运行，沿用E0已核工具版本/固定TS基线，输出目录每次新建，不能覆盖首次结果：

```sh
node scripts/bbx/check.mjs --self sdk-config-file-events --out NEW_TS_SELF_DIR
node scripts/bbx/fixtures/sdk-config-file-events/verify.mjs --self-test NEW_TS_SELF_DIR/sdk-config-file-events.ts-1.json
node scripts/bbx/fixtures/sdk-config-file-events/verify.mjs --self-test NEW_TS_SELF_DIR/sdk-config-file-events.ts-2.json
```

当前`check.mjs:61`的self侧文件后缀为ts-1/ts-2，仍需核对本次manifest的实际完整场景集合。也可明确使用两个独立`run.mjs ts SCENARIO ARTIFACT`，对两份产物分别verify，然后`diff.mjs A B --self`。须同时满足：采集exit0、独立verify通过、双轮自比SAME；只有diff SAME不够。

通过后，在新的产物目录用封存的Rust源码/binary跑同场景，并分别验证TS和Rust原始产物，再比较。保存场景/verifier SHA、基线完整SHA、候选源码/binary身份、命令、Node版本、平台、首次错误及stderr。若TS正式实际行为不同，应先定位场景/装置/基线事实，保留第一份失败，再修改本私有候选；不得添加忽略来吞掉必需callback。

## 负控

`verify.mjs --self-test ACTUAL_ARTIFACT`对已经通过正向验证的真实产物作内存副本变异，不写回原文件，13项全部必须被拒绝：漏FileChanged、created变updated、错误filePath matcher、stale selector、错误owner header、跨session callback、注销后多发callback、漏ConfigChange、重复模型工具副作用、错误落盘文件、隐藏timeout、少正式step、必需assertion没执行。

真实产品级负控计划（由E0统一调度，不在本机运行，不改参考材料/公共标准）：

- 对候选Rust临时变异关闭SDK FileChanged专用dispatch：模型write_file仍成功，但必需callback正向断言必须失败。
- 对候选Rust临时变异丢掉ConfigChange同bus接线：三次settings实际落盘，但A首callback必须缺失并失败。
- 对候选Rust临时变异移除session注销：close A后第二/三次settings多发callback，精确count与owner断言必须失败。
- 对候选Rust临时变异将relative matcher误用absolute，或将SDK事件state跨session共享：对应matcher/隔离断言必须失败。

负控源码/二进制必须与正式候选分开封存，并由E0记录实际身份及退出码；未运行负控不能说已KILLED。当前本私有fixture只准备了上述严格判据，尚无正式BBX成绩。
