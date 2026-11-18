# P0：基础能力上游增量审阅

本批审阅 `state/iface-upstream-review.json` 中配置、HTTP、字符串兼容、历史投影等 18 个路径。结论是：可以选择性吸收若干缺陷修复；不能用上游文件整体替换 backend62。backend62 已有正式宿主、历史投影和安全边界，整文件覆盖会丢掉这些能力。

本批只做源码审阅，没有修改产品、运行 Cargo、执行 TS 探针或记录验收通过。下文“等价覆盖”仅表示在明确输入范围内源码实现等价，“缺口”表示候选需要新增实现及验证；均不能转换成接入已完成。

## 受审身份与判读方法

| 身份 | 提交 |
|---|---|
| 共同祖先 B | `8a7b0626a068b011c9ed70025c2231f3fb725bd8` |
| 产品起点 D | `33b23a773b437aaea33696292810cf67aa54fdef`（backend62） |
| 受审上游 U | `ca7c7cc33db3c42cc81d78239657f3dfbdc422ca` |
| 产品行为基线 T | `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` |

先读 B→U 的生产 hunk，再对照 D 同位置和替代实现。D→U 的全部差异不能当成上游新增：其中很多实际上是 D 独有能力。Rust 路径下文省略共同前缀 `src/pilotdeck-gateway/src/`；`U:path:line` 和 `D:path:line` 均指上述 Git 对象中的一基行号，不指不断变化的工作区。T 的只读 checkout 为 `/Users/da/ws/pilotdeck-rs/resources/PilotDeck`，本批实际核对了其 HEAD 等于 T。

## 逐路径处置

| 路径 | 生产增量与依据 | 对 backend62 的判断和本轮处置 | 所需验收 |
|---|---|---|---|
| `config/binding.rs` | U:12 保留 Node 路径参数 TypeError；U:261、272 保留 YAML 解析/转换错误类；U:473 抽取统一强制转换检查 | D:12、244 确实会丢错误类；但本 hunk 的具体入口是 StaffDeck 绑定及 SOP bundle，本期范围外，明确暂缓。强制转换 helper 属等价归一，可随通用 helper 一起处理，不据此扩展 StaffDeck 范围 | 后续交接：非字符串 bindingId、读文件竞态、YAML 解析、alias 限制失败后原 reload diagnostics 不被错误替换；需跑基线 |
| `config/mod.rs` | U:522 的 `read_error` 为 Node 文件错误保留 `Error` 类；schemaVersion 强转改用 helper | D:523 仍默认 `PilotConfigError`，通用错误类是真实缺口，选择性接收待实现。helper 等价。保留 D 的 proxy 配置及已有宿主解析，不整体替换 | errno/code/message/name；已有诊断后再触发非配置错误，诊断保持；schemaVersion 的不可转换对象和深数组 |
| `config/sections.rs` | B→U 只有 alwaysOn.language 改用统一 helper | D 已有同样的检查和 `coercion_error`，等价覆盖；不把 U 的其他旧结构覆盖回来 | language 的 TypeError/RangeError 映射不变；不回退 D 的 model reference、继承 provider 判断 |
| `config/store.rs` | U:816、846 新增 `WatchStop` 并暴露 watcher panic；U:1178、1445 新增同次采样的 content-unknown twin；U:1330 合并 obscured 分支；目录迭代借用路径 | D:797 吞掉 join 结果，D:1167 仍可能把读取失败的未知内容提交为新指纹，是真实缺口。选择性接收 watcher 结果和 twin 修复，待实现。借用路径和重复分支合并可一并归一；测试 hook 不是产品完成证据 | 读失败但内容未改时不多发 reload，恢复也不多发；真实 metadata/content 变化仍报告；stat/read_dir 失败与内容失败交替；启动失败、目录/文件切换、正常 stop、真实 panic 的返回分类 |
| `config/value.rs` | U:58 把 `native_tag().unwrap_or("Object")` 改为 `js_value::object_tag`；U:174 新增 `require_string_coercible = string_coercible.map_err(coercion_error)` | D:35 的字符串化及 D:157 的检查/错误映射已提供同样行为，等价抽取，按 config 消费者一起复用；不另记能力缺口 | Set/Object 标签、不可转 primitive 的 TypeError、深数组 RangeError 与原消费者一致；不只验证 helper 自身 |
| `http_proxy.rs` | U:308、449、775、832、1100 把未 flush 的响应尾部纳入读门和 header 超时窗口；另有四处 console 错误行改为 best effort | D:385 只看 in_flight，D:718 flush 没清理独立写窗口；真实缺口，选择性接收待实现。D 含正式 Native HTTP 共用连接逻辑，该修复不能因文件名 proxy 被判为仅 TS 兜底。日志修复复用已有 console helper | 慢/背压 socket 下响应尾部未 flush 时不能提前读下一请求头、不能向已有响应插入 408；真正 flush 后可接下一请求；body release 与 Drop 双到达不能重置 pending；坏 stderr 后仍回 502/正常完成连接 |
| `js_value.rs` | U:99 `code_point_chunks`；U:264 `object_tag` 统一原有语义 | helper 名称尚无，但 D:module_host.rs:762 已有同样高低 surrogate 配对；D:config/value.rs:35 已保持 native_tag。属等价抽取，允许按消费者需要复用，不当成新增产品能力 | 普通 BMP、有效 surrogate pair、孤立 high/low、空串；Set/Object 字符串化；抽取前后事件边界相同 |
| `lib.rs` | SIGXFSZ 注册失败诊断从 eprintln 改用 console helper；其余新增 fixed-port 模块为测试 | 诊断坏写可导致 panic，真实缺口，选择性接收日志行待实现；测试模块只在适配相关装置时接收。保留 D 独有 extensions/image/native_component 等模块 | handler 注册失败且 stderr 不可写时不得因打印再次失败；正常诊断文本保持 |
| `main.rs` | 启动 banner、cwd、accept/连接失败行改为 best effort | D 仍用 println/eprintln，真实缺口，选择性接收待实现。保留 D 的 process-start 登记；cwd 获取错误仍应传播，不把环境错误一并吞掉 | stdout/stderr 关闭或坏写时 listener 仍接收请求；普通输出字节保持；真实 current_dir 失败退出行为保持 |
| `module_host.rs` | B→U 仅把 UTF-16 字符串流事件遍历换成 `code_point_chunks` | D:762 等价覆盖，选择性抽取即可。U 的 resolver 返回类型及缺少 production_config 是 D→U 差异，不是新修复；不得丢 D 正式 RuntimeHost 和 resolver 错误传播 | 同一字符串分块与孤立 surrogate 替换一致；resolver 权限拒绝仍返回原错误 |
| `native_archive.rs` | U:140 拒绝含孤立 surrogate 的 sessionKey，注释明确是已登记 TS 差异 | 行为冲突，暂缓直接接收；T:createLocalGateway.ts:1620 仅检查 `trim()`。D 有正式 archive 授权、共享 transcript/storage 和 artifact 接口，必须保留。历史上游差异单不是本候选批准凭据 | isolated surrogate 与显式 U+FFFD 是否映射同一文件的基线/本候选探针；安全修复若接收须新增本候选差异证据，保证拒绝发生在任何读写前 |
| `native_http.rs` | B→U 仅增加 `native-http-served-head-cl0` 负向控制注入点 | 正常生产行为无增量，暂缓装置注入，待相关 HTTP 对拍适配。不能顺手搬 U 整文件：会移除 D:591 loopback token Host/Origin 门及正式 module-host 接线 | HEAD /health 的真实 framing；负向控制必须使对拍失败；维持 D 的 local-token 与 module-host 验证 |
| `session_model.rs` | U:158 同样拒绝孤立 surrogate 的 sessionKey | 行为冲突，暂缓；T:NativeSessionModelSelectionPort.ts:118 只有空白 gate。与 archive 一起处理 session 身份策略，不能在不同入口零散拒绝 | 模型选择读写、archive、session 创建的相同键矩阵及不误碰另一会话数据 |
| `skills/import.rs` | B→U 仅测试 Cleanup::drop 忽略清理错误，生产函数没有变化 | 无生产缺口，暂缓测试装置变更。D:416 使用 node_io::copy_file 是 D 自身增量，不能被 U 的 fs::copy 覆盖 | 如复用该测试，注入主断言失败+清理失败仍保留首次失败；不需要为本 hunk 重改产品 |
| `storage/mod.rs` | U 新导出 `WebHistoryView/project_web_history/message_contains_tool_result` | D 已有 session_projections 与 web_messages 的正式链路，出口名不同不能判缺功能。不接收另一套同职历史投影出口；下行的语义差额补到 D 已有实现 | 正式 transcript→hydrate→web/session 入口覆盖；同时保留 D 的 blob/replacement/invocation-log 接口 |
| `storage/transcript_adapter.rs` | U:1389 把 message.some 检查抽为返回 bool；U:2164、2330 增加五类 WebHistory 计算以及 JS length/filter/spread 失败语义 | D:1533 的 hydrate 检查等价；D:session_projections.rs:222 已提供正常消息、artifact、status/error、budget、incomplete-turn 投影。真实差额是畸形 artifacts 的 JS 运行语义，选择性接收待实现到已有投影；不要整文件替换、更不要引入第二个历史事实所有者 | 见下节；正常历史、坏行、compact、artifact 字符串/对象/null、tool-result 短路、对象身份及顺序都经真实入口验证 |
| `switch.rs` | 文档说明 CronManager 共享 stop latch；生产只改 abort-rejected 的日志行 | 日志坏写是真实缺口，选择性接收待实现。共享 latch 是否已接入由 runtime 审阅批次判断，不能依据本处注释标完成；保留 D 的 drain/entries timeout 语义 | abort 返回失败且 stderr 坏写仍完成预期 switch 结算；不把 latch 注释当一次性 stop 证据 |
| `upstream.rs` | U:49 upgrade task 错误行改为 console helper，其余为测试 | 真实诊断 panic 缺口，选择性接收待实现；这属于仍保留入口的错误处理，不扩展依赖 TS 的执行路径 | HTTP upgrade 失败+stderr 坏写时连接任务正常释放，原错误消息及协议行为保持 |

## 历史投影的具体边界

D 的 `storage/session_projections.rs:236` 已按 transcript 累积 WebHistory，`storage/web_messages.rs:1201` 实际消费；它不是只有接口声明。D 的 `storage/transcript_adapter.rs:1533` 也已对 hydrate 的 `message.content.some(...)` 做了 null、非数组、短路校验。U 的 `message_contains_tool_result` 抽取不构成另一项缺失能力。

但 D 的 `session_projections.rs:258、309` 用 `array_of` 处理 artifacts。T 的 `src/session/projection/WebHistoryProjections.ts:78` 先读取 `artifacts.length`，随后在 `:90` 按 turn 是否已有 tool result 决定是否调用 `.filter`，最后展开迭代器。代码可以直接推导出以下差异，尚待实际基线探针确认：

1. 同 turn 含 tool result、artifacts 为非空字符串：T 最终按 JS code point 展开；D 的 array_of 会把它当空数组丢弃。U:2479 的 code-point 分支补足这一语义。
2. 无 tool result、artifacts 是有正 length 的非数组：T `.filter` 失败；D 静默跳过。不能把“坏数据没有 crash”当成保持行为。
3. artifacts 数组中的 null、compact metadata 缺字段等错误必须保留触发时机和第一错误。只在纯 helper 上测试不足；D 的 hydrate 门可能已经提前拒绝某些形状，因此必须从正式存储入口验证哪些差额可达。
4. `Set` 的 `SameValueZero`、对象引用身份和 token-usage 字段插入顺序仍需单列探针。U 另建 Vec/JsObject 的实现不能仅凭覆盖了普通字符串 turnId 就认定完整等价。

D 已实现的投影可以作为 ifACE Context/历史兼容层的复用算法；会话历史写入、生命周期和提交顺序仍由计划确定的 Kernel 所有者负责。复用显示投影不意味着再建一份可写历史。

## 进入实现时的组合约束

优先组合为通用错误类、watcher twin/stop 结果、HTTP flush 窗口、无 panic 的 console 行。console helper 已存在于 D:`native_process/channels.rs:113`，无需复制上游整套 channels；main 若跨 crate 调用须沿现有公开导出安排。watcher stop 改返回值后，调用者必须处理或明确忽略其结果，不能只新增 enum 就声称 panic 可观测。

等价 helper 与消费者一起抽取，避免只加未使用函数。历史投影差额补现有 `session_projections`，保留 D 的 storage 工厂、替换恢复、授权及真实入口。sessionKey 的更严拒绝暂缓到统一身份方案和本候选差异流程，不能借上游曾登记直接认可。

以上都是 P0 接收/暂缓结论，不是 P1/P5 或联合验收结果。本轮验收记录应另外绑定实际候选提交、测试入口和产物。原上游测试或注释中的探针路径仅供找用例，不能作为本候选已运行的证据。
