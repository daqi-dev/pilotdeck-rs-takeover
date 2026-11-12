# P1 steer 附件接线恢复

日期2026-11-13；负责人h1_inventory。当前仅调查/固定TS取证，按主协调全Rust check5冻结未修改Rust、未Cargo。旧 `A7.2-STEER-ATTACH-001`与R1明确未批准且未实现，不是可继续拒绝的验收豁免。

## 最小范围与复用

产品只需 `native_process/methods/steer.rs`：保持现session/run admission先行，在其后、资源门前使用现 `native_process/services.rs::compose_attachment_turn`（pub(super)），由spawn_blocking执行实际文件读取/解析。此函数已被普通submit Services::prepare_attachment_turn复用，返回text/blocks与allowedReadFiles；无需新增Requests字段、Services API或动Cargo/公共装配。把准备结果转换为canonical content及完整白名单，经原SteerMessage.enqueue交准确run。uploadedAttachments始终忽略，不取得upload lease。

不得直接调用Services::prepare_attachment_turn：它有submit专属字符串校验和profile校验，TS steer无此校验。无附件raw message必须继续原值；不能解除attachments拒绝后丢弃附件。目标文件范围为steer.rs、methods/tests.rs中两块现unsupported断言、新private steer_attachment_tests.rs及本目录新fixture/formal BBX场景。共享services.rs、Requests、mod表不动。测试须保留admission→prepare→resource→enqueue的顺序及现竞态fence。

## 实际TS证据

固定cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85、Node22.22.0。`tests/fixtures/steer-attachments-native/oracle.mjs`直接调用真实InProcessGateway.steerTurn与真实GatewayAttachmentTurnComposer，只在router active/run结果边界捕获message/allowedReadFiles；不描述成完整server运行。源码与dist在内存转译逐字节身份核对，隔离HOME/PILOT_HOME，文件/符号链接均在新临时目录。

原始 `_tmp/20261113-steer-attachments-oracle-{a,b}`，26格两轮产物字节相同SHA256 b4021afdb238bd1f691ab5416f33700d5b5e54eda7a37e8477bb608ae695a32b；只归一化本探针根目录为<fixture>。没有启动真实模型、服务或访问18789。slot2已归还。

源码依据：`src/gateway/client/InProcessGateway.ts:1859-1883,2463-2477`；`src/gateway/dialog/GatewayAttachmentTurnComposer.ts:55-88,173-227`。已确认inline text/image、注册路径和symlink双拼写白名单、普通路径inline、缺失文件诊断、uploaded忽略及门顺序。

wire边界实测：空/缺省/null attachments与raw object/undefined/null message仍可accepted；只有产生blocks时null message才抛TypeError reading length。attachments [null]先抛reading path，number1/object{}抛attachments is not iterable；false/0/{length:0}/string x在基线实际accepted（无有效附件）。空message/数字message含块时不添加原文本。no-active/wrong-run优先于任何附件错误；admission匹配但无router resource时仍先prepare，[null]错误优先于no-resource结果。

## 待实施和验证

等主明确解冻后实施。新测试读取原始oracle，对实际mailbox canonical content和allowedReadFiles逐值断言，不能只断accepted。更新旧两块unsupported断言为状态门与真实prepare失败顺序。另提供真实WS活动turn steer→provider后续模型请求包含附件及落盘的formal场景，交E0隔离运行TS双轮与新Rustbinary，先错误附件内容/白名单剥除负控。注册文件须证明工具read_file授权由准确steer白名单可达；不能只检查路径note文字。

当前接口：已查明；实现待实现；接线待实现；验证仅TS模块层26格；交付待验证。没有把旧差异改成新批准豁免。


## 解冻实施候选

主协调封存4525bb2snapshot后明确解冻，已在批准私有范围实施：steer.rs复用同compose并精确处理raw输入；methods/tests.rs仅原两块测试改为真实内容/真实prepare错误；新steer_attachment_tests.rs新增2项，含24个实际composition/error子格（另外两格admission由原handler测试覆盖）及Requests→实际mailbox双路径read authority。原生平台Linux/mac不跳过symlink格；Windows条件排除该Unix双拼写格，未声称Windows验收。

直接rustfmt通过；没有Cargo。新formal场景turn-steer-attachments-native.json已写，沿已有turn-steer event answer配置，不改run/evidence：tool_call_started时送真实注册附件，初始bash仅一次permission答复；模型必须看到附件后读同一外部隔离路径，再返回STEER-ATTACH-OK并落盘。当前formal未运行，不能声称权限grant闭环已通过。

旧差异新增R2仅解释恢复基线、不新增accepted豁免。下一步集中check、原steer全家族/新2项及services附件回归、E0固定TSself与Rust正式场景、负控；source/binary身份固定后才验收。
