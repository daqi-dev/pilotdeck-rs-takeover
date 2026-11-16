# pilotdeck-rs/tokenizer (`pilotdeck-tokenizer`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-tokenizer`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 不可变 o200k_base 普通文本 token 计数器：忠实保留 js-tiktoken 1.0.21 缩写正则，无文件、网络或宿主权限。

提供 `count_tokens(&JsString)` 与精确的 `SOURCE_PATTERN`，为上下文裁剪、预算统计与 `pilotdeck-agent` 提供与 TS 基线一致的 token 计数。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/tokenizer@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-tokenizer`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)

## 源码边界与补充说明

仅做离线普通文本计数；不读写磁盘配置、不发起远程计数请求、不持有预算账本。

重要说明：本包是从 `pilotdeck-rs/src/` 抽取的源码包分发形式，`Cargo.toml` 使用 workspace 路径依赖 `tiktoken-rs = { path = "../vendor/tiktoken-rs", version = "=0.12.0", default-features = false }`，同时通过 `version.workspace = true` / `license-file.workspace = true` 继承 `pilotdeck-rs/src/Cargo.toml`。
