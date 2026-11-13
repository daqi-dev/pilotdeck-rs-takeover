# scripts

接手后（2026-11-10 起）判定「Rust 与基线一致」只靠两样：`bbx` 黑盒对拍，以及 `CLAUDE.md` 里的四条构建测试命令。下面列出目录里留下的东西和各自用途；上一轮流水线专用的脚本（交付巡检、评审调度、PR 闸门、命名检查、负控装置等）已删除，需要时从 git 历史找回。

## 现行验收

| 路径 | 用途 |
|---|---|
| `bbx/` | 黑盒对拍：同一场景喂给基线和 Rust，比较 WS 帧、HTTP、CLI 输出、发给假 provider 的请求和落盘文件。用法见 `bbx/README.md` |
| `ui-smoke/` | 基线的 ui/server 分别接基线 gateway 与 Rust gateway，比较全部输出。见 `ui-smoke/README.md` |
| `agents/common.md` | 主会话派给子代理的通用说明 |

## 保留的旧装置（冻结或被保留的东西引用，暂不动）

| 路径 | 为什么留着 |
|---|---|
| `parity/`、`gateway-parity/`、`ui-server-parity/` | 三套冻结对拍尺子；文件哈希登记在 `specs/*/FROZEN.md`，由各自的 `check_frozen.py` 检查 |
| `cli-parity/` | CLI 对拍装置，规则在 `specs/cli-parity/` |
| `a5-19/`、`exp008/` | 冻结尺子引用的实验装置 |
| `a6-host/` | host 夹具的录制与探针脚本 |
| `gateway-coverage/` | `specs/gateway-coverage/coverage-manifest.json` 登记的账本校验器 |
| `product-coverage/` | `specs/product-coverage/README.md` 使用的入口枚举与检查工具 |
| `retirement-outcome-parity/` | `specs/gateway-parity/diffs/A8.6-RETIREMENT-OUTCOME-PARITY-001*.md` 引用的判据实现与复算命令 |
| `smoke/` | 真实 provider 冒烟（`real_provider.py`）与 edgeclaw 冒烟；配对测试 `test-smoke-real-provider.py` |
| `cargo-slots.sh` | `smoke/real_provider.py` 开编译前调用它查 cargo 名额 |
| `test-gateway-ledger.py` | 差异账本相关回归 |
| `check-tools.sh` | 本机工具就绪检查（只报告不安装） |
