# Original53 单类参考池准入重登记

2026-11-13 用户明确批准「精确单类准入及冻结登记（推荐）」。仅当同一 UTC 日至少 20 次独立 TS 参考两两 G03 PASS 时允许单类；多类仍要求每类至少两次，非传递关系、单例、来源或身份错误仍拒绝。Rust 每遍仍须与某一确认类全部成员匹配；其他 52 场景、k=5、分母、登记锁及负控不变。历史原 REFUSED 保留，真实池仅已完成 TS-only 定标，不代表正式 5+5 通过。未观察到的合法罕见类可能使本次 Rust 被误判失败。

本地变更 ID：Original53-single-class。两家非作者已复审原事件候选及三份装置文件；本次仅更新事件中的审查引用及批准记录，不改审过的三份文件。完整 53 场景离线 controls 和正式 5+5 尚未执行；不得将机器检查当作这些步骤的替代。原有冻结登记由受控 writer 在候选提交后更新。

Calibration-Evidence: _tmp/20261113-original53-single-class-proposal/real-pool-result.json
Independent-Reviews: SDK: _tmp/20261113-original53-single-class-proposal/sdk-review.md; Protocol: _tmp/20261113-original53-single-class-proposal/protocol-second-review.json
Approved-By: 用户，2026-11-13
Approval-Basis: state/decisions-20261113.md 原始用户批准；本地 Original53-single-class 变更记录
Change: 0b6b069af4bfe855eb122f21cc18461bd829991efd6b4b8c3d9ef557b587286b 6c27c7cf2c5614c6c4a81f0d5f5fc9ae0ff3d0839860ae0f51bb5d13294fab46 scripts/parity/score.py
Change: ceaa70dfaafd3583eb43372aefbf11a60642861539ffe1ddcccdab3e1f300140 99459327a27982d3bd1eba6881c32e1027b0725e4e659f325af8981e024c2e3f scripts/parity/controls/multimodal.py
Change: e8a7b4c4025a2a3cccbabc34b3f2b3c8542f58268ccf327f5e4937e19e890742 ce0c0090e2798fb0f839ab98bb3087f7803aaff22ba2767ba8e1ea6369d32a73 specs/parity/multimodal.md
