---
id: D1-DEFAULT-NATIVE-001
---

## 类别

有意改进：按本次交付要求将 Rust 发行入口的缺省路由由历史透明代理切换为原生宿主。此条登记 Rust 迁移入口的选择变化，不豁免原生业务协议与固定 TypeScript 基线之间的差异。

## 基线依据

固定基线 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的 `src/cli/pilotdeck.ts:36` 进入 `server` 分支，`:104-115` 创建本地 Gateway，`:211-233` 将该 Gateway 交给 HTTP/WS 服务；其正式服务入口不以 Rust 历史 A4 透明代理作为后端。这里是源码依据，本条未新增 TS 运行金样，也不将其写成新的对拍结果。

切换前 Rust 的 `native_process::gateway_mode(None)` 返回 `false`，`main.rs` 在没有首参 `--native` 时进入 A4 代理；原 `gateway_mode_config_matrix` 和 `default_and_explicit_proxy_route_the_proxy_front` 钉死了这一迁移期选择。该旧 Rust 默认不是 TypeScript 对外协议事实。

## Rust 行为

`src/pilotdeck-gateway/src/native_process.rs::gateway_mode` 在配置缺席时选择 native。`src/pilotdeck-gateway/src/main.rs` 沿用既有 native 装配、参数解析与生命周期：没有 `PILOTDECK_GATEWAY_MODE` 且没有 `--native` 时直接运行原生服务；显式 `PILOTDECK_GATEWAY_MODE=proxy` 才选择历史代理。首参 `--native` 仍优先于有效的 proxy 设置；非法、空值及非 UTF-8 模式配置仍先拒绝启动。

## 范围

改变 gateway 可执行程序的缺省入口选择，并给确实需要旧代理的测试夹具加上显式 proxy 配置。为使默认进程内循环不依赖 sidecar 安装，低层入口允许省略 `--sidecar`：记录当前可执行文件同目录的约定路径，启动 native 时不检查或执行该文件；显式传入的路径仍按原规则要求文件存在并校验 UTF-8，选用 stdio 时才需要真实程序。不改变监听与鉴权、Gateway 方法、HTTP/WS 消息、持久化及停止协议；不把 proxy 当作原生失败后的自动兜底。无模式启动的测试使用隔离 home、project 和随机端口。

## 机器判据

- `src/pilotdeck-gateway/src/native_process/tests.rs::gateway_mode_config_matrix` 断言缺省 native、显式两种模式和非法值拒绝。
- `src/pilotdeck-gateway/tests/mode_routing.rs::unset_mode_without_native_flag_starts_native_and_drains_on_sigterm` 使用 `env_clear`，既不设置模式也不添加 `--native` 或 `--sidecar`；复制真实二进制到无 sidecar 的隔离目录，PATH 仅包含空目录，确保无需 Node；读取真实 native 监听公告、完成 WS 101 握手、发送 SIGTERM，并要求正常停止记录和退出码 0。该子进程保留非法 `PILOTDECK_PROXY_LISTEN`，误入代理必须失败。
- `native_process::tests::omitted_sidecar_uses_optional_sibling_and_explicit_path_is_validated` 断言省略可解析且显式不存在路径仍拒绝；既有 `non_utf8_home_and_sidecar_are_refused` 保留原边界。
- 同一真实进程测试文件保留显式 proxy/native、参数优先级、非法配置以及 proxy→native→proxy→native 演练。旧代理地址、model-wire、upgrade 与 native-process 代理格夹具显式选择 proxy，原业务断言保持。
- `src/pilotdeck-gateway/src/native_ws/tests.rs::legacy_proxy_body_unchanged` 精确识别 D1 新模块头和路由块，恢复旧模块头、去掉路由及参数 helper 后仍使用旧 SHA-256 `02613cf4a756f4e9cfdb59c5356961798244feb209bf53a5485491c8b7dd4622`，保护其余代理源码字节。它只提供源码保护，不能代替上面的进程观察。
- `tests/fixtures/mode-routing/negative_controls.py` 将缺省错误变异改为 native→proxy，仍绑定同一模式矩阵的明确断言；保留原 8 个路由变异与 2 个公告变异及统一判定逻辑。静态 `--check` 已通过；变异执行和本条新增的 Rust 测试待集中编译验证，未在本条声明通过。`native-process/negative_controls.py --check` 仍报告范围外的 `configured-hooks`、`configured-modules`、`title-cancel-wait`、`title-timeout-wait` 四个旧锚失效，此条没有放宽这些断言。

复算入口：在 `src/` 运行受影响的 `pilotdeck-gateway` lib 模式矩阵、`legacy_proxy_body_unchanged`，以及 `--test mode_routing`、`--test native_process`、`--test proxy_listen_address`；路由负控按脚本声明的约束在固定候选和独占编译槽执行。

## 批准记录

已获当前用户交付指令授权：2026-11-13 用户要求执行工作区 `plan.md`，其「已确定的架构方向」明确规定「默认发行入口必须运行 native；旧透明代理退出正式业务运行链。」本条限于这一明确的默认选择调整；未扩大为任何业务差异豁免。实际测试结果仍须按上面的机器判据独立记录，授权不等同于验证通过。
