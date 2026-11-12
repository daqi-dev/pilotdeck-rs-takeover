---
id: D1-UPDATE-CANCEL-OWNERSHIP-001
---

## 类别

明确取消后的资源残留缺陷修复。只向CLI父PID发送SIGINT/SIGTERM时，基线更新器子进程和其HTTP连接仍运行；父退出不足以证明本次命令已回收。

## 基线依据

固定 cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85 的 src/runtime/updateCommand.ts:6-25 启动更新子进程但不转发父进程信号。2026-11-13 macOS/Node22.22.0，通过 tests/fixtures/c1-entry/update-capture.mjs 执行正式pilotdeck update、隔离HOME/PILOT_HOME和临时假SQLite身份，仅连接本地随机端口Web mock。两轮各9格，自比相同。原始证据 _tmp/20261113-c1-entry/update-ts-a 与 update-ts-b 的 cancel-* 目录：对CLI PID发SIGINT或SIGTERM，父以该信号退出后200ms，ownedGroupExists和mockApplyConnectionOpen仍true；对整个前台自有组发送信号，两者false。观察结束后才由采集器清理，不把外部清理计作产品行为。

## Rust 行为

src/pilotdeck-cli/src/update.rs 将该次可选updater启动到独占进程组，在spawn前安装SIGINT/SIGTERM监听。独立监督线程不依赖stdout能否写出，收到取消后将信号转发至明确自有组，至多3s宽限（再次信号可提前升级）后发送SIGKILL，最多再5s确认leader退出并回收，之后恢复CLI首次收到信号的默认终止状态。不能确认回收时输出明确错误而不报告更新成功。使用waitid(NOWAIT)保留leader身份到最后组信号，reap之后不再对其数字PGID发信号。正常退出0/2和其他返回码映射保持。

## 范围

仅本CLI启动并仍归它所有的updater进程组、未移交后代与其HTTP连接。固定Web组件的createCommandClient明确由server持有已提交更新任务；processScope.cjs使用detached、processGuardian.cjs明确guardian可独立存活。这些已经移交的后台任务/独立guardian不在取消组内，本修复不宣称撤销远端更新事务，不改变排除的ui/server协议，不按进程名杀进程。默认Rust核心仍不依赖Node。

## 机器判据

src/pilotdeck-cli/tests/c1_update_exit.rs 使用隔离安装布局、本地helper和随机TCP连接，分别测试仅向CLI PID及其前台组发送SIGINT/SIGTERM，要求原始父signal状态、updater/附属进程终止、连接关闭；看门狗仅操作测试自己创建的组。正常argv/cwd/环境、双流输出、Unicode、返回码9项继续回归。超时保存fixture、输出计数和明确PID供归因；不靠延长30s测试期限遮蔽原始失败。实际结果见docs/backend-delivery/C1.md，登记不代表尚未运行的取消测试通过。

## 批准记录

2026-11-13 用户docs/plan/backend-completion.md明确授权挂起、丢数据和安全问题修复，并要求成功/失败/取消恢复及资源释放证据。C1正式基线两轮确认取消后本命令子进程和连接残留，按此授权修复；Web已移交任务的既有所有权边界保持，未扩展为取消服务器任务的新功能。
