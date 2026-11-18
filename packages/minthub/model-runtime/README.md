# pilotdeck-rs/model-runtime (`pilotdeck-model-runtime`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-model-runtime`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 模型执行运行时基础（无环境网络权限）：异步网络取消/定时器仲裁、重定向策略、请求头与 ModelStreamError/ModelHttpTransport。

共享异步模型驱动与网络仲裁基础：`network`（`ExchangeFuture` / `AbortFlag`）、`error::ModelStreamError`、`transport`（`ModelHttpTransport` / `ModelBody`）、`fetch_redirect`、`headers`、`http` 与 `services`。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/model-runtime@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-model-runtime`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/model-core` (`=0.1.0`)

## 源码边界与补充说明

不包含 Hyper client、DNS、socket、TLS、代理、CA 文件、凭据读取或审计落盘；不向 ifACE 协议暴露 Node 私有事件分类。

附带 `source-provenance.json` 记录从 Gateway 抽取模块的来源 SHA-256 与验证矩阵范围。
