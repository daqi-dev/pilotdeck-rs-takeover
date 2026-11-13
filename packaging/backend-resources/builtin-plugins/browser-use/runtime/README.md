# 可选 browser-use 源码依赖运行时

此目录提供独立装配与验证脚本，不包含 TypeScript 私有实现或预编译浏览器。默认 plugin.json 保持原状；没有安装运行时或浏览器时不得把目录存在等同于能力已供给。

固定基线 cc7c4e6 锁定官方 `@playwright/mcp@0.0.75` 和 `playwright` / `playwright-core@1.61.0-alpha-1778188671000`。本目录重新声明公开依赖，完整性值见 pnpm-lock.yaml / sources.json。装配仅使用已有公开 pnpm store 的源码包，禁生命周期脚本、禁浏览器下载、排除可选 fsevents；实际 LICENSE / NOTICE 与所有安装文件摘要写入输出 runtime-manifest.json。Node 和已安装 Chrome 是外部运行时依赖，版本与身份须随交付单独记录。

```sh
node provision.mjs /absolute/NEW-runtime /absolute/existing-pnpm-store
node list-tools.mjs /absolute/runtime /absolute/NEW-catalog-evidence
PILOTDECK_BROWSER_SMOKE_SLOT=authorized node formal-smoke.mjs /absolute/sealed-pilotdeck /absolute/runtime /absolute/NEW-formal-evidence
```

`authorized` 仅表示调用方已取得协作轻量槽，不是用户权限凭据。不要并发启动未排槽的浏览器测试。所有输出目录必须全新；失败证据不可覆盖。生产打包需把装配后的 runtime 作为可选资源，并设置正式 loader 已支持的 PILOTDECK_RUNTIME_ROOT；本脚本不修改全局资源 manifest。

`list-tools` 只执行真实 stdio initialize/tools/list，不能证明浏览器启动。`formal-smoke` 通过正式 Rust server、Gateway WebSocket、native 模型/工具循环与 builtin plugin loader 调用实际官方 MCP。临时资源 spec 明确使用已安装 Chrome、headless、perSession；HOME/PILOT_HOME/TMPDIR、模型 HTTP、HTML 页面、Gateway 端口全部隔离，不 attach CDP，不访问真实账号或用户 profile。模型只允许本轮已知 browser 工具的权限请求；通过真正的 permission_decide 回答。

验证顺序为 navigate、本地表单 type、snapshot、click、snapshot、close；第二个 snapshot 必须包含 `Hello, MCP source runtime`，最后 native assistant 必须输出 `BROWSER_NATIVE_LOADER_OK`。工具名来自实际 provider 工具目录。此版本在当前 MCP 初始化下返回 Snapshot 文件链接；装置只读取 receipt 实际声明且 realpath 位于本次输出目录内的 YAML，保留原工具 receipt。不能将该结果描述为“模型收到内联 snapshot”。

macOS 首次实际成功证据：`_tmp/20261113-browser-use-formal-sixth/`；封存 CLI SHA256 `415ab93e1e21f1237ceec3e31f05a6f79067cdffe3a7976410ebe3b461c6ed4b`（root-thirteenth，d0 源身份，不代表最终 release），Node v22.22.0，已安装 Chrome 157.0.8103.78。运行时为 `_build/browser-use-runtime-20261113-first/`，190 个安装文件，无 native 动态库或可执行下载。实际 catalog 23 工具，formal 六次工具调用成功、server 自然退出0。Chrome PID20258及其观察到的子进程在 browser_close 后消失；MCP PID20190和server PID20181在shutdown后消失；隔离 profile `pilotdeck-mcp-browser-use-fZdSna` 已由正式客户端删除。首轮至第五轮装置失败均保留，未称产品失败或抹去失败。

边界：仅证明这台 macOS 已安装 Chrome 与该 sealed CLI 的本地 HTML 能力。不覆盖 Linux 浏览器供给、发布包搬运、真实网站登录、远程浏览器、下载/上传、全部23工具或默认 chromium 配置可用性。默认产品资源配置没有为测试改变。
