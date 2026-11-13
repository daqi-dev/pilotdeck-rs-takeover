# 正式后端真实模型 journey runner

这是 opt-in 的最小 smoke runner，运行指定的封存 Rust CLI，通过真实 Gateway WS、工具审批和模型 HTTP 请求完成任务。没有自动运行、构建或下载步骤。数据和独立结果检查位于相邻 `data/`；runner 不用预定模型回答代替真实响应，不以固定自然语言字符串判正确。

当前交付支持两条任务：

- `sales-margin`：读项目 CSV 与口径，计算并落盘结构化结果和业务报告。数值 oracle、来源、源文件保护与自然语言质量分别检查。
- `renewal-after-restart`：第一轮读取合同并在会话记住用户决策、不写业务文件；server 完整退出后用同一 session、同一持久 home 重启，第二轮依真实历史核算并写结果。独立 oracle 检查第二轮实际 provider history，不能因第一轮请求本身含决策就判恢复成功。

其他数据任务不属于本 runner 当前支持的执行范围。`permissionPolicy` 与 `cancelWhen` 专用任务会被拒绝；普通超时已有 abort/退出处理，不等于专用取消场景交付。这里不承诺 OS 沙箱或隔离任意 shell 的能力。

## 运行要求与无网络检查

需要支持内置 `fetch`、`WebSocket` 的 Node（已在 Node 22 使用）、可执行的封存 Rust CLI、Unix `ps`，以及明确授权的 Anthropic-compatible messages API。上游 endpoint 必须是完整 messages URL，例如 `https://provider.example/v1/messages`。计数 endpoint 固定取同一 origin 的 `/v1/messages/count_tokens`。当前没有 OpenAI 协议适配分支。

先检查代码与 manifest；这不会读取凭据、启动产品 server 或调用模型：

```bash
node --check scripts/smoke/backend-journeys/run.mjs
node scripts/smoke/backend-journeys/run.mjs \
  --manifest scripts/smoke/backend-journeys/data/manifest.json \
  --scenario renewal-after-restart \
  --permission-mode structured-only \
  --check only
```

## 显式启用与预算

执行者自行在进程环境注入凭据，runner 不读取 `.env`。下面的 endpoint/model 是占位示例，请替换；`JOURNEY_API_KEY` 应由执行环境注入，值不要出现在命令参数、日志或入库文件中。`JOURNEY_BIN` 和 `JOURNEY_BIN_SHA256` 必须指向此次实际要验证的封存 CLI 及其精确 SHA256；不要用未经确认的新构建替代。

```bash
export JOURNEY_ENDPOINT='https://provider.example/v1/messages'
export JOURNEY_MODEL='model-name'
export JOURNEY_BIN='/absolute/path/to/sealed-pilotdeck'
export JOURNEY_BIN_SHA256='the-exact-sealed-cli-sha256'
export JOURNEY_ROOT="$PWD/_tmp/backend-journey-smoke"
mkdir -p "$JOURNEY_ROOT"
```

对于**全新且尚无任何请求**的预算，从 0 初始化。独占创建会拒绝覆盖已有状态；重复任务、连接探测和其他共用预算的真实请求必须累计，不能用此步骤重置计数。如果已经做过连接探测，应由执行者把真实消耗计入已有预算再执行，不能假称从 0 开始。

```bash
python3 - "$JOURNEY_ROOT/budget.json" <<'PY'
import json, pathlib, sys
p = pathlib.Path(sys.argv[1])
with p.open('x') as output:
    json.dump({
        'requestsUsed': 0,
        'knownInputTokens': 0,
        'knownOutputTokens': 0,
        'unknownUsageRequests': 0,
        'pendingRequests': [],
    }, output, indent=2)
    output.write('\n')
PY
```

显式 opt-in 的完整例子：

```bash
node scripts/smoke/backend-journeys/run.mjs \
  --manifest scripts/smoke/backend-journeys/data/manifest.json \
  --scenario renewal-after-restart \
  --permission-mode structured-only \
  --binary "$JOURNEY_BIN" \
  --binary-sha256 "$JOURNEY_BIN_SHA256" \
  --output "$JOURNEY_ROOT/renewal-001" \
  --endpoint-env JOURNEY_ENDPOINT \
  --model-env JOURNEY_MODEL \
  --token-env JOURNEY_API_KEY \
  --budget-state "$JOURNEY_ROOT/budget.json" \
  --max-provider-calls 40 \
  --allow-real authorized

python3 scripts/smoke/backend-journeys/data/verify.py \
  --manifest scripts/smoke/backend-journeys/data/manifest.json \
  --scenario renewal-after-restart \
  --run "$JOURNEY_ROOT/renewal-001"
```

换为 `sales-margin` 时同时使用新的 output，预算文件保持同一个。output 必须不存在。runner 的 `runnerPassed` 只代表协议/生命周期完成；`oracleVerified` 始终由 runner 置为 false，必须另跑独立 verifier。`passed_with_manual_review` 不是 `fullyVerified`，仍须看来源、shell/外部访问和自然语言解释是否一致，不能只凭 JSON 数值或 `completed` 判任务完整成功。

预算上限是累计最多 40 次真正上游 admission、已知 input 400000、output 20000。title、路由分类、生成、计数与 HTTP 失败均占请求数；输入计入报告的 cache read/create。main 单请求最多 2048，只有真实 system 严格匹配固定 title prompt 首句且无 tools 时最多 4096，thinking 必须明确 disabled。产品配置关闭 transient/zero-usage retry，代理 `fetch` 不重试，也不修补不合预算的请求 body。

预算文件用独占 `.lock` 目录避免多个 runner 并行共用；admission 前先持久计数。崩溃后的 `pendingRequests` 表示未结算消耗，不能归零或假称未发出。恢复时先核实原进程已退出并审查状态，再由执行者处理遗留 lock；runner 不自动解除另一个 owner 的锁。

## 单并发转发、计数与失败记录

产品合法的 title/main 并发在代理有界 16 项等待队列排队，上游 singleflight。保存排队与 admission 时间；客户端在 admission 前断开时移除等待项、不耗上游请求额度，保留取消收据。请求超限、队列过载和未知 route 都保留失败，不变成模型成功。

每个收到的 method/path 在分支前写 `received-routes.jsonl`，不保存 headers 或 query 值。只有 `POST /v1/messages` 与 `POST /v1/messages/count_tokens` 受支持。产品 provider base 使用 proxy origin，分别由原生生成和 token-count consumer 拼接 `/v1`，避免版本段重复。

`kind=token-count` 请求不套生成的 max_tokens/thinking 断言，但受相同 request cap、队列、超时与总预算约束。HTTP status/response 原样返还；真实计数端点 404 等 unsupported 结果由产品自身走估算 fallback，代理不会合成成功 token count。缺 usage 明确记录 unknown，不能计作 0；计数结果的顶层 `input_tokens` 本身不是生成账单 usage。

## 权限和生命周期边界

`structured-only` 模式下所有真实审批默认 deny。第一轮 `noBusinessWrites` 时全部 deny；其他阶段仅在 `write_file` 的结构化路径精确属于 `scenario.expect.files`、解析目标与真实祖先均在项目内时 allow。保存 request、answer、decision 理由及响应原帧。bash、agent/subagent、execute_code 等审批不靠解析命令放行。没有审批的只读工具仍由轨迹独审；路径检查不是 OS 沙箱，也不证明没有未经观测的外部访问。

项目使用正式默认 general＝独立 `PILOT_HOME`，另建独立 `HOME` 和 `TMPDIR`。恢复任务保存同一 session/home，退出 server、检查已观测 owned PID 全部消失与 TCP socket 关闭后才重启。超时发送真实 `abort_turn`，等待终态或结算 deadline，任务仍记失败，然后清理 server。停止失败会留下失败记录；不会把硬杀结果冒称正常退出。

## 参数与证据

| 参数 | 含义 |
| --- | --- |
| `--manifest`、`--scenario` | 数据文件及其中受支持的任务 ID |
| `--permission-mode structured-only` | 必须显式指定的审批模式 |
| `--check only` | 只做 manifest 基本检查，随后退出，不访问 API |
| `--binary`、`--binary-sha256` | 封存 CLI 路径与精确身份，运行前强校验 |
| `--output` | 新的隔离运行目录 |
| `--endpoint-env`、`--model-env` | 保存完整 Anthropic messages URL、model 的环境变量名 |
| `--token-env` | 可选 API 凭据环境变量名；指定后要求值存在，仅转发时使用 |
| `--budget-state` | 所有共用额度的运行复用同一个累计预算 JSON |
| `--max-provider-calls` | 请求总上限，默认 40，必须为 1–40 的整数 |
| `--provider-timeout-ms` | 上游单请求超时，默认 90000ms；必须为正数 |
| `--allow-real authorized` | 实际执行必需的明确 opt-in |
| `--skills-dir`、`--plugins-dir` | 可选、明确控制的资源目录；不是这两条任务的必要参数，也不因此扩展已验证范围 |

输出包含 `journey.json`、`turn-N.json`、`provider.json`、`received-routes.jsonl`、`trace/events.jsonl`、`processes.json` 与 server 日志；provider 按 turnIndex 保存实际 requests/messages/responses/usage，文件只列实际哈希与大小。loopback 上游标 `realProvider:false`，只能作本地 mock/replay 诊断，不能当真实 LLM 成绩。

上游凭据只在 runner 转发时从执行者指定环境变量读入，不传入产品 child，产品只获得本地 proxy 的虚拟 key。日志不保存 headers、Authorization 或 `process.env`，持久文本按已知凭据值脱敏。证据仍包含自造项目内容和模型文本；本脚本不是任意敏感业务资料的通用脱敏器。

本文件不包含私有 launcher、真实运行证据或凭据。运行状态与验收结论以统一 `state/backend-handoff.md` 为准。
