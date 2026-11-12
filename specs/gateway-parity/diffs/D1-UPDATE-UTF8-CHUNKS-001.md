---
id: D1-UPDATE-UTF8-CHUNKS-001
---

## 类别

明确数据损坏 bug 修复。更新进程输出的一个UTF-8字符被管道读取边界分开时，基线逐块解码会把合法字符替换为U+FFFD。

## 基线依据

固定 cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85 的 src/runtime/updateCommand.ts:16-22 对每个stdout/stderr Buffer独立toString并转发。Node22.22.0/macOS 本轮只使用隔离HOME/PILOT_HOME及本地子进程mock；src/pilotdeck-cli/tests/fixtures/c1-update/baseline-probe.mjs 可重跑。原始产物 _tmp/20261113-c1-update/baseline-output-verified.jsonl。完整中文/emoji及100KB文本正确；将“中”的前两UTF-8字节单写，20ms后补其余，基线转发“��文🙂\n”（两个替换字符）；非法字节a ff e4 b8输出a��。

## Rust 行为

src/pilotdeck-cli/src/update.rs 对stdout/stderr各自增量有损解码，暂存不足一字符的尾部，在后续字节到达时完成解码；EOF残片和真正非法字节继续替换为U+FFFD。双流仍合并转发至CLI stdout，子进程退出0/2映射CLI0、其他退出/信号映射CLI1。

## 范围

只改变合法UTF-8字符跨读块时的输出损坏。不添加新的更新协议、自动重试或安装布局；Web updater仍是已有Git Web安装的可选外部管理组件，ui/server不在本轮Rust迁移范围，默认Rust核心不依赖Node。不同流的独立解码状态不得拼接。

## 机器判据

update模块测试遍历“中文🙂”所有字节分割；src/pilotdeck-cli/tests/c1_update_exit.rs 使用隔离安装目录、本地fake node和正式Rust二进制，覆盖中文双流大输出、8192读边界、非法字节与EOF尾部，并继续验证argv/cwd/环境/stdin EOF/返回码。基线probe内置固定SHA与Node版本断言；实际Rust结果见 docs/backend-delivery/C1.md。登记不代表未运行的正式入口测试已通过。

## 批准记录

2026-11-13 用户 docs/plan/backend-completion.md 明确允许修复丢数据缺陷；本轮独立非作者审查发现8192读块会破坏合法中文，修复按该授权实施。只接受本条精确分块损坏修复，普通CLI输出兼容要求保持。
