---
name: 20261120-remote-process-discipline
description: 远程进程只按记录的 PID 杀、kill 前先 ps 枚举、非死锁不 kill -9 编译器（缓存损坏重编译）、构建锁等自然退出——模式匹配 pkill 在多任务并行时必漏必误伤
metadata:
  type: feedback
---
2026-11-20 final47 收尾在 mbp2 上连出四次自伤，全因用模式匹配 pkill 管理远程进程：

1. 想省事用门禁包装跑单个 e2e 测试 → 白跑 10 分钟全仓 check+clippy（该直接 `cargo nextest run -p <crate> -E 'test(...)'`，秒级）。
2. 停包装时 `pkill -f cargo-clippy` → 没匹配到 `clippy-driver`（名字不同），孤儿编译进程占着构建目录锁。
3. 清孤儿时 `pkill -9 -f "cargo "` → 把自己刚启动的 direct 跑的 cargo 一并杀死（exit 101，日志只留 "Blocking waiting for file lock"）。
4. kill -9 打断编译 → 增量缓存半成品损坏 → cargo 重编 pilotdeck-transport/tool-core/gateway 等大 crate（~10 分钟）。

每步都是「选当下最快动作、未枚举副作用」。正确路径四律：

- **只按记录的 PID 杀进程**。spawn 时记 PID，杀时用 PID；模式匹配 pkill 禁用（命令行字符串匹配在 cargo-clippy/clippy-driver 这类近名进程上必漏，在 "cargo " 这种宽模式上必误伤己方）。
- **任何 kill 前先 `ps` 枚举匹配项**，确认清单里没有自己启动的任务。一条 ps 的成本远低于一次误杀。
- **非「确认死锁」不用 kill -9 打编译器**：代价是增量缓存损坏 + 大 crate 重编译；多数「挂起」是等锁或 dyld 校验慢，等自然退出更便宜。
- **构建锁优先等持有者退出**；确需杀锁持有者时，先确认它属于哪个自己启动的任务。

背景：当天 mbp2 处于 dyld/syspolicyd 故障后状态（测试二进制首启动校验极慢），nextest 挂起多为环境而非死锁——止损动作应当比对「等待」本身更保守。
