# Cyclic YAML and prototype merge boundary

固定基线 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`、Node 22 的真实
`loadPilotConfig({env, projectRoot})` 记录，读取自造 global 与 project 两个 YAML 文件。
所有输入都是 YAML 数据，使用 YAML alias 表达共享/自环，没有 JavaScript 函数或用户数据。
这些图不能用普通 JSON 树表达，但它们属于基线实际 YAML 配置入口。

三个有限输入区分：

1. global 的 unused 自环与 project 独立的 `unusedProto.__proto__`：成功，仅两个 unknown-field warning。
2. 两源的 unused 自环在实际 object/object merge 路径相遇：精确 `RangeError: Maximum call stack size exceeded`。
3. project 在同一个 unused 分支放 `__proto__` 自环：成功，仅 unused warning；存在 prototype 路径不等于发生无限 merge。

`record.mjs` 记录完整 YAML、实际 diagnostics/agent/sourceKinds 或 error，以及固定 TS source closure、Node 和记录器 SHA。
原始两例与追加重合边界的私有采集保留在本轮 `_tmp/20261113-ph01-cycle-proto/`。
这份 oracle 不声称任意 YAML 图/原型组合已经验收，也不把真正递归错误降级为空成功。

复现使用显式 Node 22、固定 TS 的 tsx，并将 HOME/PILOT_HOME/TMPDIR 各自指向自造目录：

```sh
PDRS_BASELINE=/absolute/path/to/PilotDeck \
HOME=/isolated/home PILOT_HOME=/isolated/pilot TMPDIR=/isolated/tmp \
/absolute/path/to/node22 --import /absolute/path/to/PilotDeck/node_modules/tsx/dist/loader.mjs \
src/pilotdeck-gateway/tests/fixtures/config/cycle-proto-merge/record.mjs /isolated/output
```

Rust `config::tests::cyclic_yaml_sources_with_proto_follow_actual_merge_paths` 复用这些 YAML，
经过真实 native 文件读取、解析与合并，各跑 role-limited CLI load 与 full gateway snapshot，比较完整 diagnostics、选择与 agent snapshot 或精确错误。
该 Rust 测试待集中候选编译/执行；记录器的实际成功不代替 Rust 测试结果。
