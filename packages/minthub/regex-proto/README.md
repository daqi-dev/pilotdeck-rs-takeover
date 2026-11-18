# pilotdeck-rs/regex-proto (`pilotdeck-regex-proto`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-regex-proto`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

基于 rquickjs（QuickJS-NG）的有界 JS RegExp 原型引擎（#1265）：默认关闭特性，仅按 UTF-16 码元与执行预算执行正则匹配。

把 JS RegExp 原型隔离在非默认 `regex-proto` feature 后，确保默认 workspace 构建无需拉取或编译 `rquickjs`；启用特性后提供带内存与步骤预算的 `RegexEngine`。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/regex-proto@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-regex-proto`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- 无内部 `pilotdeck-rs/*` 或 Kernel 包依赖。

## 源码边界与补充说明

原型实验包，不暴露任意 JS 求值、宿主 I/O 或模块加载；默认构建下仅提供 `engine_disabled_reason()`。

启用正则引擎需打开 Cargo feature `regex-proto`（`rquickjs = "=0.14.0"`）。
