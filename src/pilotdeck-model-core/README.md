# PilotDeck 模型协议纯核心

本包从 backend62 的既有实现中抽取，不依赖 Gateway、HTTP 客户端或 Kernel 私有实现。目前公开的能力为：

- UTF-16 工具参数 JSON 解析及 `jsonrepair`，保留是否修复标记、孤立代理码元与既有深度上限。
- Anthropic、OpenAI Chat Completions／Responses、Google 的用量和结束原因规范化。
- 公共模型请求体清洗与非流式响应形状判定。
- `codec` 的完整 Anthropic、OpenAI Chat Completions、OpenAI Responses、Google 请求构造、thinking/schema/cache/message/tool 投影与协议端点。
- 四协议有状态事件转换：text/thinking/工具参数累计与修复、签名、usage、finish reason、原始错误/响应字段；保持 UTF-16 累计上限。
- 不读取凭据的协议请求头构造，以及精确 `request_started` 指纹事件。
- `driver` 的原生重试次数/延迟判定、流检查点与续写请求、流错误分类、时长/重复片段保护，以及 UTF-8 增量解码与 SSE 解析。
- `request::inspect_request_media` 只读检查完整 canonical 请求中的消息内容，供调用者决定是否需要媒体输入能力。

Gateway 的正式 provider 路径通过薄适配层消费本包；原算法实现已经移出，不保留第二份副本。OpenAI Responses 继续使用 OpenAI 用量语义；Google 的缓存扣减和思考 token 仍按其原有协议处理。

本包不负责鉴权、HTTP/SSE 传输、重试执行调度、令牌计数、会话与 Kernel `ModelSessionFactory` 安装。普通非 Google 流使用 `codec::StreamState`；Google 的 `codec::providers::google::StreamState::normalize(&wire)` 直接接收原始 wire JSON，并在内部执行一次 SDK 投影；调用者不可再次预先调用 `sdk_response`，否则 citation 等非幂等字段可能丢失。独立 `sdk_response` 入口供审计和非流式响应投影使用。传输完成条件仍由对应宿主循环处理：Google 在 `message_end` 停止，非 Google 会继续读取 EOF 和可能迟到的 usage；传输中断不能合成为正常完成。

`driver` 不读取时钟、随机数或环境，也不发起下一次请求。宿主显式传入时间、随机值和 `LITELLM_MAX_STREAMING_DURATION_SECONDS` 原始值；Gateway 保留原 attempt 循环、网络/取消/定时先后与审计路径。`Checkpoint::partial_text()` 保留 UTF-16，`StreamGuard` 返回原始错误名与消息，`errors::ErrorRef` 借用 canonical 错误以避免在分类时复制 raw。`sse::SseBuffer::with_limit` 的限额单位是 UTF-16 码元，不能当作宿主的输入字节预算。这里共享的是纯决策与状态，不代表完整异步 HTTP 驱动已迁出或已接入 ifACE。

媒体检查返回 `Result<bool, MediaInspectionError>`。它要求根对象、`messages` 数组和 canonical 的 user/assistant 消息，按原严格 admission 的字段规则验证九类内容及 timeline；tool_result 内仅接受 text/image/pdf。image/pdf/audio/media_reference 返回 true；tool_result_reference 不读取落盘正文，即使 MIME 提示为纯文本也保守返回 true。未知类型或字段结构不完整时返回独立的 `Unsupported { path, reason }`，不会伪造 provider 错误码。metadata、schema、tool input/raw 和额外字段保持 opaque，不扫描其中看似媒体的标签，也不进行 JSON/UTF-8 规范化。它不是完整请求验证器：路由、provider 能力和权限另行检查，调用者必须封组并执行同一份已检查输入。此前宽松 admission 可以保存为 BoundaryOnly 的未知内容在此明确不受支持；metadata 等 opaque 字段导致的 BoundaryOnly 不等同于未知内容，不会因此被拒绝。

凭据覆盖位于 Gateway 薄适配层。`provider_headers_with_auth` 与 Google `headers_with_auth` 只接收由可信调用者提供的临时修改 closure，以维持既有自定义头合并、鉴权插入及协议头后处理的准确顺序；纯核心没有读取 `apiKey` 或访问环境变量的路径。公开 `provider_headers`/Google `headers` 不注入鉴权，自定义头由调用者负责选择。

错误使用 `codec::CodecError`，保留 Request 的 code/message/details、Provider 的完整 canonical object，以及 Thrown 的 name/message；Gateway 逐字段映射回原 `ModelStreamError`，不改错误合同。不能依据这个包编译通过声明整个模型能力已完成宿主接入。

`tests/fixtures/normalization.json` 是固定 TS 基线 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的实际执行输出，包含 52 条用量、64 条结束原因、4 条请求清洗数据。来源文件哈希和 Node 版本保存在夹具中。重新采集使用：

```sh
node --experimental-strip-types tests/capture-baseline.mjs <固定基线目录> tests/fixtures/normalization.json
```

采集器只读取基线模块，不启动产品、不读取凭据、不调用模型。原 Gateway 的 86 条 jsonrepair／Anthropic 事件对拍保留原入口并调用本包实现。

完整 provider 的原有单测随算法迁入 `codec/providers`，夹具以原字节随本源码包携带在 `tests/fixtures/provider-codec/`；manifest 保存原 Gateway 路径、固定 backend62 身份和 SHA256，两个 model-stream 夹具中的完整 TS sourceCommit/sourceSha256 表原样保留。Responses 的历史夹具只有短提交与 generator 标记，没有原运行逐文件 hash 表，这个历史证据限制未被补造元数据掩盖。用例包含 Responses 9 个请求、Google schema/prototype/citation、thinking 与累计边界。局部 provenance helper 保留固定提交、非空有效 source hash、可选 PDRS_BASELINE 的 HEAD/工作区/提交与工作文件 hash 校验，不依赖 Gateway 测试代码，不携带原再生/进程装置。原 Gateway 流/完成 HTTP oracle 保留在原宿主入口，继续验证相同核心。`codec::limits::LimitGuard` 与 Gateway 原测试入口共用同一限额，使限额负向控制覆盖迁出的状态机。

`tests/fixtures/driver.json` 直接执行同一固定 TS 基线的 `RetryPolicy.ts`、`StreamingCheckpoint.ts` 和 `continuationRequest.ts`，包含 30 条重试、9 组逐事件检查点、6 条续写输入及其真实输出。夹具边界显式编码 NaN、无穷大、负零与 undefined；比较器保留对象顺序和 UTF-16。负向控制故意改变延迟、代理码元和缓存失效字段，确认同一比较器拒绝这些差异。采集命令为 `node --experimental-strip-types tests/capture-driver-baseline.mjs <固定基线目录> tests/fixtures/driver.json`。SSE 原有 3 个测试随实现迁入；Gateway 原 HTTP 行为测试继续通过薄适配层覆盖这些共享函数。
