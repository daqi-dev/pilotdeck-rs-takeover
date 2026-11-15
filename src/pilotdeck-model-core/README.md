# PilotDeck 模型协议纯核心

本包从 backend62 的既有实现中抽取，不依赖 Gateway、HTTP 客户端或 Kernel 私有实现。目前公开的能力为：

- UTF-16 工具参数 JSON 解析及 `jsonrepair`，保留是否修复标记、孤立代理码元与既有深度上限。
- Anthropic、OpenAI Chat Completions／Responses、Google 的用量和结束原因规范化。
- 公共模型请求体清洗与非流式响应形状判定。

Gateway 的正式 provider 路径通过重导出消费本包；原算法实现已经移出，不保留第二份副本。OpenAI Responses 继续使用 OpenAI 用量语义；Google 的缓存扣减和思考 token 仍按其原有协议处理。

这不是完整的 Model 能力包。完整请求构造、带状态的流式转换、鉴权、HTTP、重试、令牌计数和 Kernel `ModelSessionFactory` 接线仍由相应运行时负责，不能依据本包通过而声明整个模型接入完成。

`tests/fixtures/normalization.json` 是固定 TS 基线 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的实际执行输出，包含 52 条用量、64 条结束原因、4 条请求清洗数据。来源文件哈希和 Node 版本保存在夹具中。重新采集使用：

```sh
node --experimental-strip-types tests/capture-baseline.mjs <固定基线目录> tests/fixtures/normalization.json
```

采集器只读取基线模块，不启动产品、不读取凭据、不调用模型。原 Gateway 的 86 条 jsonrepair／Anthropic 事件对拍保留原入口并调用本包实现。
