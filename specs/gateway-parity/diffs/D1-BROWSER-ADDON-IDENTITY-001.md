---
id: D1-BROWSER-ADDON-IDENTITY-001
---

## 类别

已批准的显式发行 addon 身份修正。避免独立 browser 插件与默认普通技能使用同一 `name@source` 注册键后，插件 MCP 和安装技能被覆盖。只改本轮独立编写 addon 的 manifest ID 与必要包装记录，不改固定基线或 Rust registry 的覆盖语义。

## 基线依据

固定 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的 `src/extension/plugins/loading/PluginLoader.ts:14-25` 以普通技能目录 basename 形成插件名；`src/extension/plugins/runtime/PluginRuntime.ts:552-556` 将 builtin 插件和普通技能共同刷新，`src/extension/plugins/runtime/PluginRegistry.ts:73-75` 以 `name@source` 最后写入覆盖。使用首失败安装中的实际 addon 和完整默认技能运行固定 TS 纯库的原件为 `_tmp/20261113-ra07-ts-registry-diagnostic-first/result.json`，SHA256 `0c17f6d902c8eb6c5a0848cc492caa63540e6bb7ca26aac9d43541d8de71bf9d`：`refreshNext` 包含两个 `browser-use@builtin`，无 staged failure，registry/session lease 最后仅保留普通技能，MCP 为空。此为纯库诊断，非正式 CLI 或网络 parity。

原正式默认安装首失败证据 `_tmp/20261113-addon-journeys/browser-run-first/manifest.json` 及 `provider-requests.json` 保留：main provider 只有 27 个内建工具，无 browser MCP；不能用本条回写首失败为通过。

## Rust 行为

`packaging/backend-resources/builtin-plugins/browser-use/plugin.json` 的 `name` 由 `browser-use` 改为 `pilotdeck-browser-runtime`。`scripts/backend-release/resource_layout.py` 保留普通 `name == dirname` 校验，仅增加目录 `browser-use` 与 ID `pilotdeck-browser-runtime` 的精确允许映射，并保留非空字符串及 ID 唯一性。外部依赖说明与安装 metadata 使用新 ID。

## 范围

仅新显式 browser addon 的插件 ID、配置启用键和身份归属显示发生变化。禁用键为 `extension.builtinPluginsEnabled.pilotdeck-browser-runtime: false`；旧 `browser-use: false` 不再禁用新 ID，不增加旧键兼容 alias。

安装目录和归档 selector 仍为 `browser-use`，技能 namespace 仍为 `browser-use:browser-use-install`，MCP server key 仍为 `browser-use`，工具名、runtime marker/参数、32 个默认技能、普通技能名及 registry last-write 行为不变。不授权任意 ID/路径不匹配，不改变 baseline 资源，不改变 core 默认零 Node 或外部 runtime 的供给边界。

## 机器判据

受影响 `scripts/backend-release/test_resource_layout.py` 验证新实际资源 ID、32 技能和真实 addon 归档搬迁及字节变异拒绝；与 `test_release.py` 合计 26 项实际通过。精确包装身份轻控制 `_tmp/20261113-browser-addon-id-application/packaging-identity-controls.json` 共 11 项：普通匹配和唯一既定 pair 接受；无关 mismatch、reverse、路径式 ID、空值、非字符串和重复新 ID 拒绝。

候选业务使用原 sealed ccc 二进制和私有 v2 addon，实际 browser 工具/Chrome/owner 结果由 `_tmp/20261113-addon-journeys/candidate-v2-first-summary.json` 单独记录；不是修改 core 源码或测试标准后获得的结果。默认未安装 addon 的 core/full 原成绩与 addon 结果分别报告。独立字节审查及固定提案身份见 `_tmp/20261113-browser-addon-id-review/review.md`；实际应用前后四文件 SHA 和基线逐字节一致性见 `_tmp/20261113-browser-addon-id-application/application.json`。

## 批准记录

2026-11-13，本会话主协调者明确转述用户已批准 browser v2 ID 修正及配套修改，并授权将已经独审的精确候选正式应用。批准对象为 review SHA256 `74cba978845d346ab88ed76ce1b6efcde3179471c3bffb62244c72933b8413de`、包装 patch SHA256 `ead125244d848c0fe0fde60e84702e37ce9e6efe2fc7cdd28482c2b9d46d0dee` 的 ID-only 方案；原 v1 目录重命名不实施。授权及新禁用键也记录在 `_tmp/20261113-browser-addon-id-application/application.json`。此记录不是单凭文档存在推定人批，不扩大为任何其他普通协议差异或正式发布授权。
