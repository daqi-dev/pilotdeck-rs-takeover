# pilotdeck-rs/tool-core (`pilotdeck-tool-core`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-tool-core`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 内置工具 `get_current_time` 纯产品逻辑：输入 schema、时区回落与时间格式化输出投影。

封装 `get_current_time` 的纯产品定义与执行逻辑（`definition()`、`timezone()`、`execute()`、`TimeOutput`），使用 `pilotdeck-transport::system_clock` 做时间格式投影。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/tool-core@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-tool-core`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/transport` (`=0.1.0`)

## 源码边界与补充说明

纯逻辑包；宿主时区发现、系统时钟读取与解析器所有权均由调用方（如 `pilotdeck-tool-iface`）注入。

仅依赖 `pilotdeck-protocol` 与 `pilotdeck-transport`，无运行时宿主或网络依赖。
