# TCP sidecar 正式入口验证

`tcp-sidecar.mjs` 从真实 `pilotdeck-agent-loop-sidecar` CLI 启动 TCP listener，使用隔离 HOME、动态 loopback 端口与确定性的宿主模型答复。它不替换默认 factory，不连接外部模型账户。成功路径必须实际经过 model.prepare、model.stream、连续事件、成功终态及另一连接上的 completed status；默认能力、同实例/新连接代际同时断言。

```sh
node scripts/transport-validation/tcp-sidecar.mjs \
  --baseline /path/to/fixed/PilotDeck \
  --rust /path/to/pilotdeck-agent-loop-sidecar \
  --out _tmp/YYYYMMDD-R2/tcp-compare --self
```

输出目录必须不存在。manifest 记录源码快照、固定 TS commit、TS 实际 sidecar/lockfile、Rust binary、场景/helper 哈希和运行末尾产物未改变检查。原始报文和进程退出留存；失败返回非零，不覆盖旧结果。TCP 前台进程应一直运行到本脚本对自己创建的 PID 发出 SIGTERM，其他退出方式判失败。

`--mutate-answer` 故意让确定性模型返回错误文本；该负控必须 exit=1、passed=false，证明两侧即使共同产生错误答案也不会被自比误判通过。

`--orphaned-call` 验证 `C-TCP-NONRESUMABLE-EOF-001` 的挂起修复。它在真实 model.prepare 尚未答复时关闭唯一绑定 socket，再从另一连接读取状态并提交原 execute。TS 必须仍 running 并拒绝 active 重复；Rust 必须 resolving/result_unknown 并 final RESULT_UNKNOWN。两侧 callback 均只出现一次。差异在专门场景中逐侧断言、原值留存，不添加通用归一化。线程回收、显式可恢复同实例的 exact callback 回放和旧 socket 撤权由 `pilotdeck-transport` 的 `tcp::tests` 使用真实 sockets 补充覆盖。

`--active-repeat` 在首个执行等待模型回调时，经另一连接重发同一 execute；两侧必须明确返回 OPERATION_ALREADY_ACTIVE，原执行照常完成且模型只派发一次。

`--completed-repeat` 验证 `C-TCP-COMPLETED-REPEAT-001`。原执行完成并查询 completed 后重发同一 envelope：TS 必须新建 stream 再完成一次模型调用，Rust 必须原样返回原终态且不新增模型 dispatch。独立 `*-provider-dispatch.jsonl` 留存实际派发次数，断言分别为 TS 两次、Rust 一次。这只证明有界缓存保留期内同实例相同请求的防重复行为，不代表跨重启或永久幂等。场景变体互斥。
