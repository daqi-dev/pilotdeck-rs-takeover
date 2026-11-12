# H1 正式挂载后的负控迁移

本轮实现已先落地：Signal、Mattermost、HomeAssistant，以及Email、DingTalk、Discord、Matrix、Telegram、WhatsApp均由production_loader_mounts构造。UNMIGRATED仅保留qq/wecom严格true拒绝。此迁移不修改冻结评分、接受差异或历史测试成绩；本次只完成静态锚检查，尚未运行mutant编译或执行，不能宣称KILLED。

旧九个loader渠道的rejects_unmigrated_*拒绝测试由对应production_loader_mount_builds_*_with_js_truthiness替换：实际调用生产mounts与load_enabled_channels，13格enabled值分别核对不拒绝、无构造错误、精确构造key/空集合。它们只证明真实构造与loader接线，不代表外部协议或正式server收发通过。固定基线及各适配器协议证据由各自交付记录承担。

本矩阵原7行保持。新增20行：Email/DingTalk/Discord/Matrix/Telegram/WhatsApp各自mount删除（静默skip）、constructor返回Err、重新插回拒绝表共18行；另2行改变真实loader enabled条件（收紧strict true、禁用仍构造），每行六个per-key positive测试全部须失败，不采用any_cell。最终27行，每行保留固定字节锚与精确断言标记。

原九个渠道拒绝断言不再保护已迁移行为；现正式mount正测保持注册、输入真值、构造错误、skip及拒绝回归负控。每个模块自身的协议特征与收发测试仍须独立运行。

检查：python3 -I negative_controls.py --check，27 unique anchors checked。未运行Cargo或mutant，不能把静态锚通过记为产品验收。
