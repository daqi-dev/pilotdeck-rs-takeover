---
id: H1-QQ-CHUNK-LIMIT-001
---

## 类别

bug 修复：QQ `maxMessageLength <= 0` 可使同步分块循环无法前进，阻塞消息处理及事件循环。仅修复这个挂起条件，不调整正常正整数分块规则。

## 基线依据

固定 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的 `src/adapters/channel/qq/QQChannel.ts:47` 直接接受 `maxMessageLength`，`:390-406` 在 `splitText` 中按该值切片。输入 `abc`、上限0时每轮追加空块而剩余文本不变；上限-1时最终剩余单字符也不再前进。

真实固定TS源码内存转换后执行原类方法，驱动 `src/pilotdeck-gateway/tests/fixtures/h1-qq/hang-oracle.cjs`；隔离 HOME/PILOT_HOME、无网络。原始证据 `_tmp/20261113-h1-qq/limit-hang-oracle.jsonl` 分别记录：上限2返回 `["ab","c"]`；0和-1进入方法后100ms没有返回。仅父进程终止自己的worker，exitCode=1为故意终止的挂起证据，不是TS正常通过或正常退出。没有修改或重写基线分块逻辑。

## Rust 行为

`src/pilotdeck-gateway/src/channel/qq/mod.rs` 的 `configured_message_limit` 将配置中的有限非正数归一为1；构造器对可表达的上限0同样归一为1。`src/pilotdeck-gateway/src/channel/qq/wire.rs` 的 `chunk_text` 对直接调用的0也强制至少1个UTF-16单元，因此 `abc` 产生 `["a","b","c"]` 并返回。

## 范围

只适用于QQ消息分块的非正上限及其挂起修复。正常正整数上限继续按基线的UTF-16计数、30%换行阈值、只消费一个边界LF、顺序发送及多块500ms间隔执行。不得以本条豁免正常请求、正文、顺序、消息ID、错误或持久状态的差异。

## 机器判据

正向断言 `channel::qq::tests::nonpositive_message_limit_cannot_hang_chunking`：配置0和-1归一为1，直接以0分块 `abc` 必须精确返回三个单字符块；正整数2000保持2000。`identify_and_chunking_preserve_qq_wire_shape` 保留正常分块规则的独立断言。

复算命令：隔离 HOME/PILOT_HOME 后运行 `node src/pilotdeck-gateway/tests/fixtures/h1-qq/hang-oracle.cjs` 获取真实TS正控与两格挂起结果；Rust由统一Cargo槽运行 `cargo test -p pilotdeck-gateway --lib channel::qq::tests`。本条追加时Rust修正后的检查/测试尚未复验，不因登记差异而自动判绿；第一轮root check exit101的原始失败证据保留在 `_tmp/20261113-c1-resume-validation/root-first-errors.txt`。

## 批准记录

用户2026-11-13明确授权按 `docs/plan/backend-completion.md` 执行；该文件“已明确的用户决定”写明“正常协议、参数、输出和持久化行为保持兼容；明确的崩溃、挂起、丢数据和安全缺陷可以修复”（当前第31行）。本条属于这项已授权挂起修复。根协调者在当前会话进一步明确派发“为QQ maxMessageLength<=0授权在specs/gateway-parity/diffs新增窄挂起修复条目”。本记录落实现有授权，不扩大正常行为差异范围；实现验证仍待统一检查。
