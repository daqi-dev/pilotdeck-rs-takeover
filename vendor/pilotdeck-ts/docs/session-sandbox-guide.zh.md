# PilotDeck 会话沙箱说明

本文面向用户、部署者和工具开发者，说明 nsjail 会话沙箱的实际行为。适用版本为 `codex/session-sandbox-nsjail` 分支，实现提交 `05c15a2ee`；Linux 验收环境为 jinan40。

## 一个服务，各自工作

一个 PilotDeck/Gateway 实例可以同时服务多个用户、运行多个会话。启用沙箱后，每个会话有自己的工作目录、用户目录、临时目录、包和缓存。agent 可以写代码、运行 Bash、安装用户包，但不能通过这些能力读取或修改另一个会话的私有文件。

例如，A 和 B 都创建 `/workspace/result.json`，两个文件背后对应不同的存储目录，内容互不影响。A 安装 Python/npm 包、修改环境变量，也不会改变 B 的环境。

**隔离需要部署者显式启用。** nsjail 是可选宿主模块；未配置 session provider 的旧部署继续使用原来的执行方式，不能按本文认定为已隔离。SDK 调用者不需要、也不能通过会话请求选择 nsjail 或指定宿主挂载路径。

## 会话里能做什么

| 能力 | 启用沙箱后的行为 |
| --- | --- |
| 读写、编辑文件，搜索、glob | 操作当前会话允许访问的文件；其他会话和宿主私有路径不可访问 |
| Bash/shell | 命令在当前会话沙箱内执行，管道和派生进程也受隔离约束 |
| Python、Node、`execute_code` | 使用基础环境中的解释器，访问当前会话的文件和用户包 |
| pip、npm | 支持会话级安装、配置和缓存，不修改共享系统环境 |
| apt、dpkg | 禁用，不能通过它们安装系统包或修改系统包数据库 |
| 后台任务 | 绑定当前会话，其他会话不能查询、取消或读取任务输出 |
| 本地 MCP、LSP、command hooks | 进程通过当前会话执行接口启动 |
| HTTP/SSE MCP、HTTP hooks、网页工具 | 使用当前会话的受控网络接口 |
| 附件、artifact、历史、plan、spill | 按会话归属处理，上传和导出不能绕过文件边界 |
| 子 agent | 默认继承父会话的执行环境 |
| 浏览器 | profile、Cookie、缓存和下载目录按会话隔离 |
| fork/clone | 创建新的执行身份和独立可写文件副本，后续修改互不影响 |

实际可用能力还取决于部署配置、工具是否启用、rootfs 是否包含相应程序，以及工具权限限制。表格不表示每个部署默认开放全部工具。

`danger-full-access` 等工具权限模式不会解除 nsjail 边界。已配置 provider 时，沙箱启动失败会明确报错，不会自动改为宿主执行。

## 文件和环境属于谁

A/B 看到的路径名可以相同，但对应不同的宿主存储目录。

| 路径或用途 | 归属 |
| --- | --- |
| `/workspace` | 当前会话的项目文件 |
| `/home/agent`，即 `HOME` | 当前会话的用户配置、环境文件和用户包 |
| `/tmp`，即 `TMPDIR` | 当前会话的临时文件；保留会话存储时也随路径保留 |
| pip/npm cache | 当前会话的安装缓存 |
| browser profile/download | 当前会话的浏览器状态与下载 |
| artifact/spill/task output | 当前会话的产物和工具输出 |
| control | 宿主管理的身份、generation 等数据，不作为可自由访问的 guest 目录 |

基础 rootfs 提供公共程序，以只读方式使用。用户不能写共享 `/usr`、`/lib` 或系统包数据库。额外系统工具由部署者预装到基础环境。

隔离对象是 agent 的代码、工具参数、工作区脚本及派生进程。PilotDeck 主服务和管理员装配的插件代码仍是可信宿主组件。nsjail 是 Linux 进程沙箱，不是虚拟机，也不隔离宿主管理员。

## 安装 Python 和 npm 包

Python 使用当前会话的 `PYTHONUSERBASE` 和 `PIP_CACHE_DIR`：

```sh
python3 -m pip install --user requests
```

npm 的 prefix、cache 和配置由本会话的 `NPM_CONFIG_PREFIX`、`NPM_CONFIG_CACHE`、`NPM_CONFIG_USERCONFIG` 指定：

```sh
npm install --prefix "$NPM_CONFIG_PREFIX" is-number
```

也可以在自己的 workspace 进行项目级 npm 安装。安装脚本和 native addon 编译仍在沙箱中执行；基础环境必须已有需要的编译工具。不能将安装目标改到共享 `/usr` 或 `/usr/local`。

A/B 可以安装同名包的不同版本，配置和缓存互不共享。包保存到各自的持久路径，正常停止后 resume 仍可使用。

**在线安装需要授权包仓库出口。** 默认禁止联网时，不能从公网下载包；本地包或离线缓存是否可用取决于会话内已有的文件。部署者需授权实际使用的仓库域名及必要的下载、重定向目标。

apt、apt-get、apt-cache、dpkg、dpkg-deb、dpkg-query 的标准入口不可执行。需要系统依赖时，应由部署者更新 rootfs。

## 搜索和自定义工具如何联网

工具已启用并配置完成、会话允许联网、目标域名和端口在出口名单中，三个条件同时满足才能联网。

默认策略为 `deny`。允许联网的会话通过自己的代理发出请求，代理检查目标域名、IP、端口和重定向。普通出口仍拒绝宿主管理 API、私网和其他会话的网络服务；将这些地址写进普通 allowlist 也不会放行。guest 不会因此获得宿主网络或自由 DNS 访问。

内置 `web_search` 和 `web_fetch` 已绑定当前会话的网络接口。搜索支持配置的 provider，包括 custom endpoint；API key 和 endpoint 仍需正确配置。搜索返回的网页链接不是额外授权，抓取该网页还需要它自己的域名被允许。

HTTP/SSE MCP、HTTP hooks 和浏览器同样使用受控出口。本机服务可以由管理员另行授予精确的虚拟 origin 到可信 Unix socket 的映射，不开放任意 localhost 或私网 TCP；agent 不能自己添加映射。

shell/Python HTTP 客户端需要支持并使用提供的代理配置。自行创建的原始 socket 不会因为设置 `allow` 就自动获得外网访问。

自定义工具通过 `bindExecutionWorld(world)` 接入，网络使用 `world.network.fetch`，文件和进程操作使用 world 的 fs、shell、subprocess 等接口。扩展注册时检查是否提供执行环境绑定。

**绑定函数不能自动约束任意插件实现。** 插件若仍直接使用宿主 `fetch`、`fs` 或 `spawn`，就绕过了这些接口。管理员需检查实际调用路径，并为该工具补充允许和拒绝场景的测试，再将其作为支持能力开放。

## 停止后如何继续

停止会话会结束执行资源、回收相关进程，不会因此删除仍在保留期内的文件、包和配置。取消一个请求也不应中断另一个会话。

resume 使用原会话的存储路径，并创建新一代执行身份。旧一代响应、事件和 task ID 不能污染新一代。可以理解为“回到原来的文件和配置，继续工作”，但不会还原已结束进程的内存、后台任务或浏览器活进程；这些需重新启动。

正常结束的 shell 将已导出的环境保存到本会话 `HOME/.pilotdeck/environment.sh`，后续 shell 和 resume 会重新加载。例如：

```sh
export PROJECT_MODE=development
```

单个程序的内存修改不会自动变成持久配置。强制杀死进程可能来不及保存最后一次环境修改，重要配置应明确写入会话文件。中断也不代表业务操作具有事务性，未完成的文件写入仍可能留下部分结果。

保留策略或用户明确删除会话可以清理持久数据。长期恢复需要保留对应 session storage，并沿用原会话身份；新建会话不会自动复用旧数据。

## 限额和常见问题

部署者可限制内存、进程数、进程 CPU 时间和活跃会话数量。达到上限时受控失败，不会转为宿主执行。一个会话触发限额不应影响其他会话的短任务。输出有大小限制，超出会截断，并支持取消。

当前没有 workspace 总磁盘配额，配置 `workspaceBytes` 会明确拒绝。CPU 限制是进程 CPU 时间限制，不代表分配 CPU 核数或带宽；内存和进程数限制需要 Linux cgroup v2 委派。

| 现象 | 先检查什么 |
| --- | --- |
| 沙箱不能启动 | Linux/nsjail、rootfs、storage、namespace 与 cgroup 条件 |
| 命令不存在 | rootfs 是否包含程序，或者是否属于禁用的 apt/dpkg |
| 文件不存在或权限拒绝 | 是否引用宿主或其他会话路径；应使用自己的 guest 路径 |
| 搜索、下载或安装联网失败 | 网络策略、域名/端口、重定向、代理支持及 API 配置 |
| 自定义工具无法注册 | 是否正确实现 `bindExecutionWorld` |
| resume 看不到原来的包 | 是否恢复同一会话、保留原 storage，并使用正确用户包路径 |
| 活跃会话容量已满 | 容量配置和已停止会话的执行资源是否完成释放 |

## 部署者如何启用

这是宿主装配选项，不是 SDK 或会话请求参数。部署者用 `createNsjailSandboxModule()` 创建模块，将 provider 和 storage root 交给 `createLocalGateway()`。一个 Gateway 共享一个 provider，各会话创建独立执行环境。

需要准备 Linux 上可运行的 nsjail、包含所需工具的只读 rootfs、持久 session storage、资源限制所需的 cgroup 委派，以及明确的工具与联网策略。基础环境或插件变动后应重新验证实际支持的能力。

模块可从宿主装配中移除，核心保留通用 `SessionExecutionProvider` 接口。移除后使用旧执行方式，同时失去本文描述的保障；要求隔离的部署不能把移除模块当作故障恢复方案。

装配示例和接口边界见[模块化开发文档](architecture/session-sandbox-development.zh.md)。

## 已验证范围

2026-10-07 jinan40 的 `full37` 完整验收返回 `overall=PASS`、退出码 `0`：67 项 PASS、1 项 apt/dpkg 明确禁用 N/A，没有 FAIL、BLOCKED 或 NOT_RUN。覆盖真实 build、单 Gateway A/B/C 并发、读写隔离、pip/npm、取消、崩溃与恢复、旧 generation、资源与网络边界、扩展能力、性能和真实模型项目 smoke。

HTTP/SSE MCP 使用本机 MCP SDK mock 验证代理链路、握手和工具调用，A/B 各返回自己的 marker；保留并通过公网 HTTP MCP 检查。3 个仅适用于 macOS 的测试在 Linux 跳过，不用于证明 Linux 隔离。

结果针对记录中的源码和环境，不表示其他部署或后来加入的任意工具已经自动验收。详细标准、复现入口和证据位置见[验收文档](testing/session-sandbox-acceptance.zh.md)。

合入 `standalone_alpha1` 后另跑 `standalone-merge39`：构建、功能回归、真实模型 smoke 和清理通过，但 cold/hot p95 未达到固定性能门槛，runner 返回 FAIL。合并版尚不能沿用 `full37` 的“完整通过”结论，具体数值和两轮证据见验收文档。
