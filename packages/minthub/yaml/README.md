# pilotdeck-rs/yaml (`pilotdeck-yaml`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-yaml`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

npm yaml@2.9.0 的忠实 UTF-16 Rust 移植：涵盖词法/解析/组合/Document#toJSON 与配置写回 stringify，保留深度保护与负零/NaN/孤立代理语义。

提供 `parse_document`、`parse_all_documents`、`Document::to_json`、`stringify::EditableDocument` 等核心 YAML 解析与编辑管线，全量覆盖 3,099 条基线差分样本与栈深度阈值测试。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/yaml@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-yaml`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)

## 源码边界与补充说明

不调用 Node、不放开基线未使用的自定义 tag/schema 选项；在 V8 实测栈深度下界处显式触发 `RESOURCE_EXHAUSTION` 保护。

本包包含移植自 npm `yaml@2.9.0` 的 ISC 许可声明（见包内 `LICENSE.yaml`）以及工作区 `AGPL-3.0-only` 声明；详尽基线调用点、栈深度测量表与差分复现说明见源码仓 `src/pilotdeck-yaml/README.md` 历史正文（本包为从 `pilotdeck-rs/src/` 抽取的源码分发包）。
