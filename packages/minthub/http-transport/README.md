# pilotdeck-rs/http-transport (`pilotdeck-http-transport`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-http-transport`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 共享 HTTP socket/TLS/SOCKS/代理/响应解码执行器：注入式拨号权限，不直接依赖 Kernel 服务。

抽取自 Gateway 的底层 HTTP/TLS/SOCKS 传输与内容解压（gzip/zlib/deflate/brotli）实现，通过拨号 trait 供环境连接器与受信 Kernel 宿主共用同一传输逻辑。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/http-transport@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-http-transport`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/model-runtime` (`=0.1.0`)

## 源码边界与补充说明

拨号权威由调用方注入；普通 `model-runtime` 与 `model-iface` 不依赖本包，不直接持有默认放行策略或凭据。

包含迁移自 Gateway 的代理回放夹具 `tests/fixtures/mcp-proxy-schemes/userinfo-oracle.json`；使用 `rustls`（ring 后端）与 `webpki-roots` 纯 Rust TLS。
