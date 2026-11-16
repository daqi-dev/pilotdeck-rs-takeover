# pilotdeck-rs/model-core (`pilotdeck-model-core`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-model-core`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 模型协议纯核心：UTF-16 工具参数 jsonrepair、四类 provider 请求/事件编解码、重试与流检查点决策、请求媒体检查。

提供 Anthropic、OpenAI Chat Completions / Responses 与 Google 四类协议的请求构造、有状态 SSE 事件累积、`jsonrepair`、`driver` 重试/检查点决策和 `request::inspect_request_media`。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/model-core@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-model-core`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)

## 源码边界与补充说明

不执行 HTTP/SSE 网络传输、不读取凭据或环境变量、不持有会话状态；鉴权头注入仅通过受信调用方的 `*_with_auth` 闭包完成。

随包携带 `tests/fixtures/`（来自固定 TS 基线 `cc7c4e6` 的 `normalization.json`、`driver.json` 与 `provider-codec/` 夹具）；Google `StreamState::normalize` 直接接收原始 wire JSON，不可预先再调 `sdk_response`。
