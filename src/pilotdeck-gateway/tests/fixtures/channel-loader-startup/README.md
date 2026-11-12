# H1 正式挂载后的负控迁移

本轮实现已先落地：Signal、Mattermost、HomeAssistant，以及Email、DingTalk、Discord、Matrix、Telegram、WhatsApp均由production_loader_mounts构造。UNMIGRATED仅保留qq/wecom严格true拒绝。此迁移不修改冻结评分、接受差异或历史测试成绩；本次只完成静态锚检查，尚未运行mutant编译或执行，不能宣称KILLED。

旧九个loader渠道的rejects_unmigrated_*拒绝测试由对应production_loader_mount_builds_*_with_js_truthiness替换：实际调用生产mounts与load_enabled_channels，13格enabled值分别核对不拒绝、无构造错误、精确构造key/空集合。它们只证明真实构造与loader接线，不代表外部协议或正式server收发通过。固定基线及各适配器协议证据由各自交付记录承担。

本矩阵原7行保持。新增20行：Email/DingTalk/Discord/Matrix/Telegram/WhatsApp各自mount删除（静默skip）、constructor返回Err、重新插回拒绝表共18行；另2行改变真实loader enabled条件（收紧strict true、禁用仍构造），每行六个per-key positive测试全部须失败，不采用any_cell。最终27行，每行保留固定字节锚与精确断言标记。

原九个渠道拒绝断言不再保护已迁移行为；现正式mount正测保持注册、输入真值、构造错误、skip及拒绝回归负控。每个模块自身的协议特征与收发测试仍须独立运行。

检查：python3 -I negative_controls.py --check，27 unique anchors checked。未运行Cargo或mutant，不能把静态锚通过记为产品验收。

## QQ / WeCom bundle 接通后的当前边界

QQ/WeCom现已加入四个实际bundle key，UNMIGRATED为空；旧拒绝表专属故障不再有可观察路径。本轮显式保留RETIRED_MUTATIONS原定义供审计，不以空循环、已退役行或静态锚成功伪称KILLED。seam中qq-dropped/rejection-silenced/message-loses-key/subset-sms-rejected/reinsert-slack/qq-wecom-gate-loosened/strict-always-true/loader-table-iteration/foreign-rejection/implemented-gains-rejected-qq共10行退役；assembly中删空拒绝调用与吞空拒绝错误两行退役。它们原来防未实现渠道静默启动；现在所有目标渠道有实际实现，由生产loader/bundle构造正测、删mount、构造Err、回插拒绝、bundle声明闭包、实际strict enabled条件保护。

新增qq_wecom_bundle_constructs_only_for_strict_true真实公共NativeChannels构造及qq_wecom_bundle_loads_mapper_and_saves_exact_state状态load/save回调测试；loader-startup新增h1-bundle-strict-gate-loosened故障，覆盖QQ/WeCom生产bundle条件。不把私有mapper单测替代正式server验证。构造顺序为F/W/Q/WC/configured；启动为F/W/Q/configured/WC；reload reconcile为F/W/Q/WC/configured，依据固定ChannelAdapterBundle.ts79-109及pilotdeckServer.ts288-296。

当前本目录--check静态数量：28。Rust测试、mutant编译/运行尚未执行；旧段落数量为迁移历史，不作当前分母。

## 历史拒绝行逐条替代清单

| 旧故障行 | 当前保护点 |
| --- | --- |
| qq-dropped（原telegram/whatsapp-dropped） | production_bundle_builders_cover_every_declared_key + h1-qq-bundle-constructor-skipped；key声明无实际构造不可过 |
| rejection-silenced | h1-unsupported-bundle-silenced保护公开fail-closed gate；全实现生产路径由actual bundle construction保护 |
| rejection-message-loses-key | h1-unsupported-bundle-loses-key + unsupported_bundle_gate_preserves_debug_error_shape |
| subset-sms-rejected | h1-*回插拒绝负控与全subset不拒绝正测；原SMS可另以回插拒绝同类故障证明 |
| reject-table-reinserts-slack | subset不拒绝正测及真实生产mount coverage；原行因空表锚移交 |
| qq-wecom-gate-loosened | h1-bundle-strict-gate-loosened（真实NativeChannels条件，覆盖两个实际builder） |
| strict-gate-always-true | qq_wecom_bundle_constructs_only_for_strict_true的false/0/null/absent格；同生产条件负控覆盖number误启 |
| loader-table-iteration | 15 loader + 4 bundle实际构造/声明闭包；QQ/WC各constructor-skipped负控 |
| rejection-table-gains-foreign-key | 空表精确pin + implemented/subset双向闭包，保留源码退役定义，不将外来key虚构为现存产品入口 |
| implemented-gains-rejected-qq | 无拒绝key后改为4bundle实际构造与声明闭包；constructor-skipped负控 |
| assembly-reject-call-removed | 空拒绝调用无行为效果；迁移到实际production startup loader-call及QQ/WC constructor-skipped负控 |
| assembly-reject-silently-skipped | 同上；真实构造/skip有固定非零分母，不把空拒绝队列作产品成功 |

当前loader-startup共32行静态锚有效，seam11、startup1；未执行mutant，所有新增保护仍待编译/运行，不报告KILLED。历史seam行另有可运行的子集，不改变冻结标准。

更新：空表回插SMS/Slack/foreign三条负控已重新锚定并保留在活动MUTATIONS（不再列作退役），当前seam14、startup1、loader-startup32。QQ/WeCom constructor skip与unsupported gate silence/lost-key均已加入活动矩阵，只有静态--check通过，未运行mutant。
