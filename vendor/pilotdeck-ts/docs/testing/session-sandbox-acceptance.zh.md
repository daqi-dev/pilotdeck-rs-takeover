# PilotDeck Session 沙箱验收规范

日期：2026-10-07。最终结论以本轮 summary.json 为准；历史诊断轮次不作为完整通过依据。

对应设计：[模块化 Session 沙箱开发文档](../architecture/session-sandbox-development.zh.md)。

面向使用者和部署者：[会话沙箱说明](../session-sandbox-guide.zh.md)。

### 最近一次完整验收

jinan40 的 `2026-10-07-full37` 返回 `overall=PASS`、退出码 `0`：68 项验收中 67 PASS、1 项 apt/dpkg 禁用 N/A，没有 FAIL、BLOCKED 或 NOT_RUN。真实 build、单 Gateway A/B/C、HTTP/SSE 本机 MCP mock、DeepWiki 公网 HTTP、性能、`qwen3.5-27b` smoke 和最终回收全部通过。Focused tests 共 82 个，79 PASS，3 个仅适用于 macOS 的检查在 Linux 上 SKIP。

原始证据和冻结源码位于 `artifacts/session-sandbox/2026-10-07-full37/`，该运行目录不纳入 Git；验收 runner、夹具及 rootfs 构建脚本纳入 Git 供复现。冷调用 p95 155.60ms、热调用 p95 137.31ms、吞吐 10.47 calls/s、supervisor idle RSS 122310656 bytes，固定门槛均通过。磁盘 quota 尚不支持，配置明确拒绝，未宣称支持。此前 `full34` 的 SSE BLOCKED 证据保留；本轮结论不覆盖历史结果。

### standalone_alpha1 合并验证

合并输入为目标分支 `babfe8313` 与沙箱分支 `33de6e66d`。冲突解决保留目标分支的 session admission、通用 invocation storage、审计失败报告、模型重试和 onboarding 限流，同时接入 session execution 注册并保留两边 invocation 回归测试及 Google 原始响应记录。

jinan40 的 `2026-10-07-standalone-merge39`：真实 build PASS，focused tests 92 个中 89 PASS、3 个 macOS 检查 SKIP、0 FAIL；另外 35 个 session factory、storage、模型和 memory 回归全部 PASS；真实模型 smoke 和清理 PASS。68 个验收项为 66 PASS、1 FAIL、1 apt/dpkg 禁用 N/A，runner `overall=FAIL`、退出码 `2`。

失败项为固定性能门槛：cold p95 基线 179.44ms、测量 322.94ms；hot p95 基线 161.08ms、测量 281.02ms，均超过 1.25 倍；吞吐和 idle RSS 门槛通过。前一轮 `standalone-merge38` 也有性能失败，并因验收目录旧的 `edgeclaw-memory-core` file dependency 导致 build 失败；离线刷新一个包后 build 恢复，源码和锁文件未为此修改。两轮证据均保留，不放宽门槛，也不将合并版标记为完整通过。性能失败原因尚未确定。

证据位于 `artifacts/session-sandbox/2026-10-07-standalone-merge38/` 和 `artifacts/session-sandbox/2026-10-07-standalone-merge39/`，包含原始日志、summary、性能样本、额外回归输出和合并源码快照。此前 `full37` PASS 只证明当时的沙箱分支与环境，不能替代本次合并验证。

## 0. 当前实现与运行方式

已接入可选 session provider、私有 workspace/home/tmp/cache/control/spill/artifact/browser roots、持久 generation、后台 owner、MCP/LSP/hooks、附件导入导出、子 agent、fork 和 session egress proxy。SDK/wire 不新增 provider 字段。真实 build 已修复 legal/invocation staging 依赖；完整验收仍必须重新运行 build。

Python/PyPI 用户包与 npm prefix/cache/config 是支持能力。apt/dpkg 六个入口显式 mask，系统根目录只读。普通 bind mount 无总磁盘配额，workspaceBytes 必须拒绝；memory/pids 使用 delegated cgroup v2，CPU 使用进程 CPU rlimit。

正式 rootfs 由 scripts/session-sandbox-rootfs.mjs 构建，包含 shell、Python、Node、rg、pip、npm、GCC/native addon 工具链、Chromium/Playwright 及动态依赖，保存 pilotdeck-rootfs.json。临时 shell-only rootfs 不能用于完整验收。

专用测试 checkout 为 jinan40:/tmp/pilotdeck-session-recovery-20261006，不修改既有生产部署。通过独立 systemd user unit 的 Delegate=yes 与 KillMode=control-group 运行 scripts/session-sandbox-delegated.mjs；该入口启用 memory/pids controllers，然后运行 scripts/session-sandbox-acceptance.mjs。每轮使用全新的 evidence 目录。

runner 顺序执行真实 pnpm build、focused source tests、独立性能阶段与当前部署模型 smoke。设置 PILOTDECK_BUILD_COMMAND 会阻止 PASS，不能覆盖构建。缺失 report 标为 BLOCKED，缺失 mandatory case 标为 NOT_RUN；最终检查 delegated root 无残余 session cgroup。只有全部必测项通过才返回 overall=PASS 和退出码 0。

默认网络 deny。允许联网夹具按域名/IP/port allowlist 使用 session 独立 Unix proxy；DNS 与目标地址校验在宿主 proxy 完成，guest 不能直接联网。公共测试服务不可用必须记录 BLOCKED/FAIL，不能放开宿主管理地址或私网获得通过。

HTTP/SSE MCP 使用 harness 在本机创建的 MCP SDK mock，分别绑定 A/B 独立 Unix socket。宿主通过 `egressLocalServices(binding)` 为每个 session 授权精确的 `http://mcp-a.invalid` 或 `http://mcp-b.invalid` origin；socket 不挂入 guest，所有请求仍经过当前 session network port/Unix proxy。真实握手、工具发现和 marker 工具调用覆盖两种 transport，并验证 guest Python 调用、伪造 owner、其他 session origin/socket、错误端口和重定向拒绝。私网及宿主管理 TCP 地址仍拒绝。保留 DeepWiki streamable HTTP 真实握手作为独立公网验收项；其失败仍报告 BLOCKED。

session shell/subprocess 累计 stdout、stderr 及 progress callback 各限制为 1 MiB，继续排空管道并标记截断；code runtime 按自身输出限额执行。文件 worker 使用独立有界捕获以支持最多 64 MiB 附件。session 关闭时 abort 文件 worker，并在异步端口的请求与结果两侧检查 generation。

关键验收入口：

| 类别 | Fixture |
| --- | --- |
| 契约、装配、非法配置与可拔出边界 | session-execution-provider、session-startup-validation、session-module-boundary、runtime/world bundle tests |
| 双向完整 roots、Python/shell/fs/rg/export、链接竞态与 host 快照 | session-boundary-matrix、nsjail-isolation |
| 单 Gateway A/B/C 工具链、附件输入/upload、fork、resume | session-sandbox-gateway-e2e |
| 取消、未知副作用、worker/Gateway 崩溃与恢复 | session-sandbox-cancellation、session-worker-crash、session-sandbox-crash |
| provider/profile 热更新、启动失败释放、retention、旧 generation | session-provider-hot-update、session-generation、nsjail-isolation |
| 浏览器 profile/Cookie/download、受控出口/HTTP hook/MCP | session-browser、session-egress |
| 资源、输出背压、capacity、pip/npm/apt | nsjail-isolation 与 resource fixture |
| 固定性能门槛与真实模型项目/artifact/resume | session-performance、session-sandbox-model-smoke |

host 初始化通过目录 FD + O_NOFOLLOW 创建路径，真实文件打开在 guest namespace 中执行，不能靠宿主 realpath 后打开。cgroup 名包含持久 storage 的设备号/inode 与 sandbox key，避免不同 storage root 同名 session 相互回收。下载快照位于不挂给 guest 的 control；授权 upload 字节通过 session 文件端口导入 artifact root。

当前 adapter 按命令启动 nsjail，没有常驻 idle worker。cold 指 provider create + 首次 jailed shell，hot 指第二次 jailed shell；idle/execution RSS 是 supervisor RSS，不能称为独立 worker RSS。20 预热、20 baseline 并冻结后再采样 20 次。固定门槛：cold/hot p95 <= baseline x 1.25，吞吐 >= baseline x 0.8，idle RSS <= baseline x 1.1。保留所有样本和 dispose p95，不放宽门槛或仅挑成功轮次。

每轮 evidence 包含 environment/config、build/focused/performance/model 原始日志、fixture reports、case-results.jsonl、host-observations.jsonl、processes/mountinfo/cgroups、performance-baseline/performance、capabilities 和 summary。保留请求、响应、身份、时间和 host 观察；缺失字段不能编造。

管理员提供的 provider/plugin JavaScript 属可信宿主代码。隔离对象是 agent 代码、参数、工作区及 MCP/LSP/hook 派生进程；此模块不隔离恶意宿主插件。插件脚本与二进制必须存在于 rootfs 或 session-owned storage，缺失时不得宿主 fallback。

## 1. 验收结论规则

核心要求：同一 PilotDeck 实例同时服务多个 session，每个 session 的 agent 不能通过任何已开放能力读取、列举或修改其他 session 的私有文件。

- `PASS`：已执行，结果符合预期，有请求、结果和宿主侧观察证据。
- `FAIL`：与预期不符，或发生任何越界读写、宿主 fallback、身份串用。
- `BLOCKED`：环境缺失、基础设施异常或测试不能可靠执行；不算通过。
- `NOT_RUN`：尚未执行。
- `N/A`：仅用于部署明确禁用的可选能力；必须验证该能力确实不可调用。

任一必测项 FAIL/BLOCKED/NOT_RUN 都不能声明完整验收通过。禁用 MCP 等可选能力可以交付受限版本，但必须在报告中列出支持范围；不能以 N/A 跳过 read、write、Bash、grep 等首版承诺能力。

边界探测返回 ENOENT 或 EACCES 均可，只要没有泄漏私有内容、目录条目或造成副作用。由权限提示提前阻断不能替代 OS 验证：必须允许沙箱内任意 shell/Python 执行测试，并确认 permission mode 不影响隔离。

## 2. Linux 前置条件

记录以下环境信息：

- PilotDeck 测试版本、provider 版本、nsjail 版本、Linux 内核和发行版。
- rootfs 标识及实际可用的 shell、Python、Node、rg 工具版本。
- namespace、UID/GID 映射、capabilities、seccomp、cgroup 和磁盘限额配置。
- 主服务启动方式、网络 profile、session 容量和测试使用的配置。
- 支持工具与明确禁用工具清单。

readiness 应先启动一个最小沙箱并确认实际隔离和限额生效。macOS 上的类型/单元测试不能替代 Linux nsjail 集成验收。运行环境缺少能力时记录 BLOCKED 原因；不切换 host provider 来获得 PASS。

所有探测在专用验收实例和测试目录进行；破坏性写入只针对验收 marker，资源压力仅针对受限测试沙箱。

## 3. 统一夹具与证据

### 3.1 夹具

启动一个 PilotDeck 服务，由真实 Gateway/session 创建链路创建 A、B；C 用于生命周期测试。禁止用两个独立 PilotDeck 服务替代并发 session 测试。

可信测试 harness 在 workspace、home、tmp、会话记录、后台输出和宿主 control 目录分别放置不同随机 marker。先验证本 session 能读写自己的允许目录，再执行越界探测。其他 session 的 marker 不放入被测 agent 提示或工具参数；仅可信 harness 知道预期值。

为 B 创建专用 `write-target.txt`，保存原始字节内容。向 A 提供 B 的宿主绝对路径和相关探测路径，但不提供 marker 内容。每轮尝试后由宿主直接比对 B 目标文件内容、文件是否存在及目录条目，不能只相信工具返回失败。

### 3.2 测试层次

| 层次 | 目的 | 建议实现 |
| --- | --- | --- |
| 契约单元测试 | provider 选择、绑定、generation、dispose、配置拒绝 | fake provider，可在普通开发机执行 |
| Linux provider 集成测试 | 内核隔离、文件/进程/RPC/资源行为 | 真实 nsjail 和测试 rootfs |
| PilotDeck E2E | 真实装配后所有开放工具不绕过 provider | 单实例 A/B 并发；可用确定性模型 fixture 触发真实工具调用 |
| Agent smoke | 验证真实模型能在隔离工作区完成正常任务 | 一个受限编程任务；不能替代确定性边界测试 |

测试入口：`npm run test:session-sandbox`。该命令执行真实 `pnpm build`；构建失败时仅为诊断继续用 source `tsx` 运行 focused tests，summary 保留 `FAIL`/`BLOCKED`，不能完整通过。显式设置 `PILOTDECK_BUILD_COMMAND` 也只会标记 `buildOverrideUsed=true` 和 `BLOCKED`。所有环境、rootfs 工具清单、配置、进程/mount/cgroup、原始输出、case JSONL、integration evidence 和 summary 写入 `artifacts/session-sandbox/<run-id>/`。未设置 `PILOTDECK_NSJAIL_ROOTFS` 或缺少 Linux nsjail 时，integration 只能是 `BLOCKED`/`NOT_RUN`，不会被当作 PASS。

### 3.3 每项证据

记录 case ID、sessionKey/sandboxKey/generation、工具和请求身份、开始结束时间、输入、结果、退出码、实际进程/挂载/cgroup 观察和 marker 比对结果。敏感测试 marker 仅留在受控验收输出中。

报告目录建议为 `artifacts/session-sandbox/<run-id>/`，包含环境说明、支持范围、逐项结果、请求结果记录和宿主侧校验记录。不要求运行者计算文件校验和。

## 4. 核心必测矩阵

| ID | 操作步骤 | 通过标准 |
| --- | --- | --- |
| CORE-01 自身读写 | A/B 各用 read/write/edit/Bash/Python 在 workspace 创建、读取、修改文件 | 本 session 内容一致，正常功能可用 |
| CORE-02 并发同名路径 | A/B 在 `/workspace/same.txt` 写不同内容，同时执行 shell 和 grep | 各自只见自身内容；输出、事件、退出码不串用 |
| ISO-01 绝对路径 | A 用 read、Bash cat、Python open、grep/glob 访问 B 宿主 workspace/home/tmp/control 路径；再 B→A | 各通道均无对方内容和目录条目，无宿主路径透传 |
| ISO-02 写入越界 | A 对 B 测试目标尝试覆盖、追加、删除、重命名和创建文件，覆盖文件工具与 shell | B 内容和目录条目保持不变；宿主 control 同样不变 |
| ISO-03 路径变体 | 对上述目标使用 `../`、绝对路径、重复分隔符及正规化后越界的路径 | 不因路径解析方式获得越界能力 |
| ISO-04 链接与竞态 | A 创建指向 B 宿主路径的符号链接；通过 shell 尝试硬链接；循环切换指向自身/越界目标的链接，调用读写及导出 | 无跨 session 读写；可信导入不引入共享可写 inode |
| ISO-05 根与挂载 | 沙箱内观察 `/`、mountinfo，并尝试访问 sessions 父目录、宿主 home/control；尝试额外挂载/进入 namespace | 无其他私有目录挂载；无法扩张可见范围 |
| ISO-06 proc 与 FD | A 探测已知宿主/B PID 的 `/proc/<pid>/root`、fd、environ；检查子进程继承 FD | 无对方文件、凭据或主服务管理 FD；仅必要本 session 能力可见 |
| ISO-07 tmp/home/cache | A/B 在相同 tmp、HOME 和缓存路径写不同 marker，再读取 | 只读到自身值；没有共享可写临时目录 |
| ISO-08 配置拒绝 | 使用不存在 provider、缺失 nsjail、不支持策略、非法挂载和失效 rootfs | session 明确失败，无任何宿主工具执行记录 |
| ISO-09 权限模式 | 在产品允许的最宽工具权限模式下重复 ISO-01/02/04 | 工具权限不解除 OS 边界 |
| ISO-10 身份伪造 | 修改工具参数/RPC 中 sessionKey、sandboxKey、宿主目录或其他 session task ID | 拒绝或忽略不可信身份；不能改绑连接或访问 B |
| ISO-11 工具绕路 | 枚举最终开放工具，逐项检查原生 fallback、直接 fs/spawn、后注册扩展和回调 | 每个本地文件/进程能力实际走当前 session；无未分类能力 |
| IO-01 导入导出 | A 上传/下载自身文件；再用 B 路径、越界链接和越界归档条目尝试导入导出 | 自身正常；无越界读写，无设备/外部硬链接注入 |
| NET-01 默认断网 | 从 A 尝试 DNS、外网、宿主管理 API 及 B 测试监听端口 | deny profile 均不可达；本 session 工具 IPC 仍可用 |
| STATE-01 控制数据 | 通过搜索、记忆、任务输出、spill/artifact 查询尝试获取 B 的私有 marker | 无 B 的私有内容；会话查询服务不能绕过文件隔离 |

对 ISO-04 的竞态使用固定测试时长和次数并记录；“没触发”只能作为该运行的证据，必须同时审查宿主文件桥是否采用无竞态的受限打开机制。不能因单次 realpath 检查通过而宣布满足要求。

## 5. 并发与生命周期必测矩阵

| ID | 操作步骤 | 通过标准 |
| --- | --- | --- |
| LIFE-01 并发创建 | 同时申请多个 session；对同一 session 并发 acquire | 不同 session 目录唯一；同一 session 一个有效 generation/worker |
| LIFE-02 请求取消 | A 同时执行两个前台请求，取消其中一个；B 持续写心跳 | 只停止被取消请求及其后代，其余请求和 B 正常 |
| LIFE-03 session 关闭 | A 启动持续派生进程/后台任务后关闭 A；B 持续运行 | A 及后代全部回收；关闭后新调用被拒绝；B 不受影响 |
| LIFE-04 worker 崩溃 | 杀死 A worker，同时保留其可能派生的子进程 | A 不再执行新任务，残余进程被回收；B 正常；无宿主 fallback |
| LIFE-05 恢复 | A 写持久文件并停止，恢复相同 session；投递旧 generation 响应 | 新 worker 读取原文件；旧事件被拒绝，且不会恢复到 B 目录 |
| LIFE-06 主服务崩溃 | 在 A 有运行任务时终止测试主服务并重启 | 识别/回收或受控接管旧 worker，禁止两个 worker 并发写同一 session |
| LIFE-07 不确定副作用 | 命令写入计数后模拟响应丢失/连接断开 | 不盲目重跑导致计数重复；报告或核对未知结果 |
| LIFE-08 重复释放 | stop/dispose 并发、重复调用；创建中途故障 | 释放幂等，不遗留挂载/进程/lease；不误删持久文件 |
| LIFE-09 热更新 | A 运行期间更换 provider/profile，再创建 B | A 固定原绑定；B 使用新策略；无进行中的降级 |
| LIFE-10 文件清理 | 对活跃 A 请求 retention 清理，再清理已停止测试 session | 活跃文件不删除；停止后按策略清理且不影响其他 session |
| LIMIT-01 资源限额 | 在 A 分别施加有界内存、进程和磁盘压力，B 执行短任务 | 已声明限额实际生效；B 不被同组终止；主服务保持响应 |
| LIMIT-02 容量上限 | 按配置启动 N 个 session 后请求 N+1 | 明确排队或容量错误，不静默无界启动；释放后可继续 |
| LIMIT-03 输出背压 | A 持续产生大量 stdout/stderr，B 产生带身份的短输出 | 输出有界、可取消，B 输出不混入 A，主服务内存不无界增长 |

同一 session 不同工具修改同一文件时不承诺自动事务隔离；测试不得把业务级写入冲突与跨 session 串写混淆。

## 6. 对外开放能力专项验收

下列能力一旦宣称支持，就必须执行；未支持时验证工具不可调用后标 N/A。

| ID | 能力与操作 | 通过标准 |
| --- | --- | --- |
| EXT-01 execute_code | Python 直接读写与通过 helper RPC 读写 B 路径；并发 A/B helper | 两条路径都隔离，RPC 不可选择其他 session |
| EXT-02 后台任务 | A/B 同时启动任务；A 查询/取消 B task ID；停止 A | owner 检查生效，仅本 session 输出可读，A 全部后代回收 |
| EXT-03 MCP | A/B 启动本地 MCP 文件工具，分别探测 B 路径及共享连接状态 | MCP 进程在各自沙箱，结果不串用；远程 MCP 另有服务端隔离证据 |
| EXT-04 LSP | 打开各自同路径文件，触发索引/诊断及越界 URI | LSP 进程、索引和 URI 路由隔离，无对方内容 |
| EXT-05 hooks/扩展 | 用 hook 和扩展工具访问 B 路径，包含失败 fallback | 沙箱内执行或明确禁止，无宿主执行 |
| EXT-06 附件/历史 | 导出附件、恢复文件历史、读取 plan/spill，尝试 B 的 ID 和路径 | 文件和元数据都按 session 授权，无任意宿主读取 |
| EXT-07 子 agent | 父 A 派生子 agent，随后尝试改绑 B | 默认继承 A；独立 session 使用独立 workspace，不能访问 B |
| EXT-08 浏览器 | A/B 使用独立 profile 下载文件并检查 Cookie/下载路径 | 无共享可写 profile/download，文件访问只落本 session |
| EXT-09 联网 profile | 访问允许目标、被禁目标、宿主管理 API、B 端口；检查重定向路径 | 允许流量正常，其余不可达；不能借网络绕过 session 隔离 |
| EXT-10 fork/克隆 | fork A 为 D，D 修改同名文件并探测 A 元数据 | 独立可写文件，无共享可写 inode，授权复制范围明确 |

## 7. 可复用探测脚本

以下脚本仅用于受控验收 marker。它们应通过真实 PilotDeck 工具调用进入沙箱执行，不能仅在宿主终端运行后当作 E2E 证据。`TARGET` 由测试 harness 替换为对方测试文件的宿主绝对路径；不要把对方 marker 内容传给被测 session。

读取探测：

```python
import os

p = os.environ["TARGET"]
try:
    with open(p, "rb") as f:
        data = f.read(256)
except OSError as e:
    print("READ_DENIED", type(e).__name__, e.errno)
else:
    # 对其他 session 私有路径，打开成功即判失败，不依赖模型判断。
    print("UNEXPECTED_READ", repr(data))
    raise SystemExit(1)
```

写入探测：

```python
import os

p = os.environ["TARGET"]
try:
    with open(p, "ab") as f:
        f.write(b"\nCROSS_SESSION_WRITE_PROBE\n")
except OSError as e:
    print("WRITE_DENIED", type(e).__name__, e.errno)
else:
    print("UNEXPECTED_WRITE")
    raise SystemExit(1)
```

每次运行后，可信 harness 必须读取 B 的原始文件并直接比较内容、存在性和目录条目。额外执行正向探测：把 TARGET 设置为 A 自身 `/workspace/probe.txt`，确认环境不是通过禁用所有文件操作来“通过”越界测试。

shell、文件工具、grep/glob、附件以及 execute_code helper 应分别执行等价探测；不能用 Python 的成功结果替代其他通道覆盖。

## 8. 性能与功能 smoke

在同一测试机、相同资源配置下记录：冷启动和热工具调用 p50/p95、每 session 空闲/执行内存、目标并发 N 下吞吐、关闭回收时延。N 至少覆盖 2，并覆盖计划部署的容量上限。数值目标在发布前由目标部署确定，未定义阈值不能标记“性能达标”。

功能 smoke：让 A/B 同时创建一个小项目、运行测试并生成同名产物，再通过各自 artifact 通道下载。验证内容正确、产物互不覆盖、会话恢复后仍可读取。记录一次真实模型运行；边界验收仍以确定性调用为准。

## 9. 验收报告模板

```text
运行 ID / 日期：
PilotDeck / provider / nsjail / rootfs 版本：
Linux / namespace / UID / cgroup / 配额环境：
实际执行命令与配置：
支持工具清单：
禁用能力及不可调用的验证：
并发目标 N / 性能阈值：

case ID | PASS/FAIL/BLOCKED/NOT_RUN/N/A | 证据路径 | 失败原因/范围

跨 session 读取或写入：有 / 无 / 未完成验证
宿主 fallback：有 / 无 / 未完成验证
残留进程/挂载/lease：有 / 无 / 未完成验证
恢复、取消与副作用结果：
性能测量及对阈值结论：
已知限制：
发布结论：不通过 / 受限预览通过（明确范围）/ 完整通过
```

最终结论以本轮 summary.json 为准。任何 FAIL、BLOCKED、NOT_RUN 或缺失可靠证据都阻止完整通过；文档不能覆盖运行结果。
