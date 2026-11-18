# t021 诊断与发行设计稿

> 任务：`cli.task.t021`「诊断与发行」。交付物：双平台发行包、诊断报告及回滚步骤。
> 本文是设计稿，不是产品验收证据。合同正本是 `state/cli-v1-operations.json` 的
> 6 个 `installation.*` 操作；引用 007 指
> [007](007_agentcpm-pd-cli-architecture.md)（v0.7.1，与 cc2c 同步副本）。
> 基线指 `resources/PilotDeck/` @ cc7c4e6。2026-11-18 定稿（S4 兼容层进程交付，
> 待 S3 集成者验收登记）。
> 边界：本轮只交付设计；compat shim crate `src/agentcpm-pd-compat/`
> （bin 名 `pilotdeck`）已并行交付，本文 §6 与它衔接。

---

## 0. 范围与总原则

1. **合同优先**：`installation.{inspect,check,apply,control_update,restart,diagnose}`
   的 input/result schema、errors、acceptance 以 operations.json 为正本，本文不发明合同字段；
   安装布局、manifest 格式、锁与代际目录是**实现层设计**，服务这些合同。
2. **不下载预编译二进制**（AGENTS.md 红线）：发行物定义为「从本源码构建出的产物
   及其清单与校验规则」。构建过程本身不下载任何预编译二进制（对齐 007 §11.3
   「不下载预编译二进制」，line 492）。
3. **双平台**：macOS 与 Linux，各 aarch64 与 x86_64（依据：基线桌面发行矩阵
   `apps/desktop/scripts` 侧 `.github/workflows/desktop-macos.yml:48-57`（arm64/x64）、
   `desktop-linux.yml:29-37`（x64/amd64 与 arm64）；Web 基线本身平台无关）。
4. **桌面壳排除**：本轮排除桌面壳与前端（AGENTS.md）。D15「桌面内置同版本 CLI」
   在本文只覆盖**接口契约**（buildId 一致性、握手 -32006 拒绝、PATH 不自动改），
   桌面打包实施不在本轮。
5. 安装根（程序所在）与数据根（`AGENTCPM_PD_HOME`，默认 `~/.agentcpm-pd`，007 §8
   line 361）是两个概念，全文分开使用。

---

## 1. 版本身份：三元组与 buildId

一切诊断、校验、回滚都锚定在同一个版本身份上。设计为三元组 + 派生 buildId：

| 成员 | 来源 | 现有依据 |
|---|---|---|
| `version` | workspace `Cargo.toml` 的 `version`（当前 `0.1.0`） | `src/Cargo.toml:35` |
| `sourceSha` | 构建时 git `rev-parse HEAD`（40 hex） | 基线 `write-web-build-metadata.mjs`（web-build.json 的 `sourceSha` 同法） |
| `contractDigest` | `OperationRegistry::global().registry_digest()` 前 12 字符 | 已交付：`src/agentcpm-pd-cli/src/lib.rs:312-320` 的 `--version` 输出 `agentcpm-pd <version> (contract <digest12>)` |

**buildId**（本设计的规范形式，进入 `installation.inspect.result.buildId`、
`installation.apply.completionResult.buildId`、`installation.restart` 的
`expectedBuildId`、握手与 ready.json）：

```
buildId = <version>+<sourceSha12>.<contractDigest12>
例：0.1.0+a1b2c3d4e5f6.x1y2z3w4q5r6
```

理由：
- 握手 `cli` 字段已有 `build`（「git 提交加内容 hash」，007 §6.2 line 173）；ready.json
  已实现 `serverRevision`（`src/pilotdeck-gateway/src/public_v1/bootstrap.rs:47-59`）。
  buildId 是这两者的统一外显，任何入口（`--version`、握手、ready.json、诊断报告）
  必须能互相对上，这是「保存版本身份」验收的核心。
- 含 `contractDigest` 使「同版本号但注册表变了」的构建不可互换——apply/restart 的
  `expectedBuildId` 因此天然承担 CAS：目标与实际不一致即 `BUILD_CONFLICT`
  （对齐基线 `ui/server/services/webUpdateService.js:141` 的 `targetChanged` 语义）。

**installationMode 取值**（实现层词表，需与 S3 集成者在注册表登记时对齐）：

| 值 | 含义 | updateSupported 默认 | reasonCode |
|---|---|---|---|
| `source` | 源码工作区构建直接运行 | false | `development`（对齐基线 `webUpdateService.js:67` 拒绝开发态自更新） |
| `managed` | 发行包安装（§2 布局，代际 + current） | true | null |
| `desktop-bundled` | 桌面内置（D15），由宿主管理 | false | `desktop`（对齐基线 `webUpdateService.js:66`） |
| `container` | 容器部署 | false | `container`（对齐基线 `webUpdateService.js:65`） |

reasonCode 词表（`updateSupported=false` 时非空；对齐基线
`webUpdateCommand.js`/`webUpdateService.js` 的 reasons 表，映射到新产品语境）：
`development`、`container`、`desktop`、`unknownVersion`、`runtimeMismatch`、
`lockBusy`、`restartRequired`、`unsupportedInstallationMode`。

---

## 2. 双平台发行包结构

### 2.1 目标矩阵与命名

| platform | arch（manifest 值） | 文件名 |
|---|---|---|
| darwin | arm64 | `agentcpm-pd-<version>-darwin-arm64.tar.gz` |
| darwin | x64 | `agentcpm-pd-<version>-darwin-x64.tar.gz` |
| linux | arm64 | `agentcpm-pd-<version>-linux-arm64.tar.gz` |
| linux | x64 | `agentcpm-pd-<version>-linux-x64.tar.gz` |

arch 值归一化沿用基线 `create-release-manifest.mjs:66-67`（aarch64→arm64、
x86_64→x64）。不含 Windows（当前范围 macOS/Linux，对齐 a040 采纳说明
「Electron安装器及Windows打包不纳入当前macOS/Linux CLI范围」）。

### 2.2 包内容

```
agentcpm-pd-<version>-<platform>-<arch>.tar.gz
├── bin/
│   ├── agentcpm-pd            # 主二进制（含 gateway 装配；一个二进制多入口，007 §3 line 77）
│   └── pilotdeck              # 可选 compat shim（同一源码、同一次构建；见 §6）
├── manifest/
│   ├── release.json           # 发行 manifest（§2.3）
│   └── compat-manifest.json   # C1 27 入口处置清单（shim crate 的 manifest 数据）
└── SHA256SUMS.txt             # 覆盖 bin/* 与 manifest/*（不含自身）
```

- shim 是**可选**成分：构建时以特性/独立目标产出；不带 shim 的发行包只有
  `bin/agentcpm-pd`。二者不得来自不同构建（§6.1）。
- 不含任何预编译第三方二进制；依赖全部静态编入（Rust 产物本身满足）。

### 2.3 release.json（manifest 格式，schemaVersion 2）

字段命名与校验规则对齐基线 `create-release-manifest.mjs:20-36` 与
`releaseService.js:47-66` 的消费者校验，新增 agentcpm-pd 特有身份字段：

```json
{
  "schemaVersion": 2,
  "product": "agentcpm-pd",
  "version": "0.1.0",
  "buildId": "0.1.0+a1b2c3d4e5f6.x1y2z3w4q5r6",
  "tag": "v2026.11.18",
  "date": "2026-11-18",
  "buildTime": "2026-11-18T00:00:00Z",
  "sourceSha": "<40 hex>",
  "contractDigest": "<12>",
  "compatManifestDigest": "<12>",
  "repository": "<渠道登记的源仓库>",
  "assets": [
    { "name": "agentcpm-pd-0.1.0-darwin-arm64.tar.gz",
      "size": 12345678, "sha256": "<64 hex>", "sha512": "<base64 88 字符>",
      "platform": "darwin", "arch": "arm64", "kind": "archive" }
  ]
}
```

- `sha256` 小写 hex64、`sha512` base64（`^[A-Za-z0-9+/]{86}==$`）、`size>0`：
  逐条沿用基线消费者校验（`releaseService.js:56-66`）。
- tag 格式沿用基线 `RELEASE_TAG = /^v\d{4}\.\d{2}\.\d{2}(?:-r[1-9]\d*)?$/`
  （`releaseService.js:4`）。
- **schemaVersion 用 2 而非复用 1**：新产品渠道与基线 Web 更新渠道独立，且字段集
  不同（product/buildId/contractDigest/kind）。旧更新器按 `schemaVersion===1`
  校验（`releaseService.js:48`）不会误读。这是设计决定，待集成者确认（§10.2）。
- `releaseId`（合同 `installation.check`/`apply` 的字段）= manifest 的 `tag`。
  渠道内 tag 唯一且指向唯一 `sourceSha`（对齐基线 `releaseService.js:47-55`
  的 manifest↔tag 一致性校验精神）。

### 2.4 SHA256SUMS.txt

每行 `<sha256>  <包内相对路径>`，格式对齐基线 `create-release-manifest.mjs:38-40`。

### 2.5 桌面内置同版本 CLI（D15）与 PATH 策略

- **契约**：桌面宿主只启动内置构建（007 §4 line 100）。发行层体现：桌面内置 CLI
  的 buildId 必须与随桌面发行的 manifest 一致；握手 `api_version` 主版本不一致
  即 -32006 拒绝（007 §6.2 line 176-178）。CLI 侧终端版本与桌面内置不同时，
  桌面只使用内置 CLI 并明确提示（007 §12 line 504）。
- **PATH**：发行包与安装器**不自动改 PATH**（C1/D11 红线，S4 计划 §0）。
  `installation.inspect` 报告安装根与 current 指向；用户自行建立
  `~/.local/bin/agentcpm-pd -> <installRoot>/current/bin/agentcpm-pd` 链接。
  发行文档给出建议命令，安装器可以提供**显式**的链接选项，默认不执行。

---

## 3. 安装布局与更新流程

### 3.1 安装根布局（managed 模式）

```
<installRoot>/                     # 建议用户级：~/.local/lib/agentcpm-pd
│                                  # （系统级 /usr/local 归 OS 包管理，另任务处理）
├── generations/
│   ├── <buildId>/                 # 一代 = 一次完整安装
│   │   ├── bin/agentcpm-pd
│   │   ├── bin/pilotdeck          # 若该代选择安装 shim
│   │   └── manifest/release.json
│   └── <previousBuildId>/         # 旧代际保留，供回滚（默认保留最近 2 代）
├── current -> generations/<buildId># 唯一的原子切换点（符号链接）
├── staging/                       # 更新暂存（空目录常态）
└── state/
    ├── update.lock                # 更新锁（mkdir 语义，见 §3.3）
    └── last-update.json           # 最近一次更新结果（成功/失败/保留的旧代际）
```

要点：
- 「准备完整新代际后原子切换」（apply acceptance）：新代在 `staging/` 里完整
  展开、校验通过后 `rename` 进 `generations/<buildId>`，此刻对运行无影响；
  最后一步 `ln -s` 新目标 + `rename` 覆盖 `current`（同目录 rename，POSIX 原子）。
- 正在运行的进程不受文件替换影响（Unix 语义），生效需 `installation.restart`
  ——`restartRequired=true`。对齐基线「apply 结束于 prepared 而非已重启」
  （a040 采纳说明；基线 `webUpdateService.js:193` `needsRestart: true`、
  `update-web.mjs` 无 `--restart` 时输出 "Update prepared. Restart PilotDeck
  manually to load the new version."，`webUpdateCommand.js:129`）。
- 数据根（`AGENTCPM_PD_HOME`）不受代际切换影响；代际目录只放程序与 manifest。

### 3.2 installation.check（检查，零安装零重启）

execution=async、retry=idempotentSubmit、acceptance「网络失败、签名不可信、
无更新分别报告；检查不安装或重启」。

流程：
1. 读本地安装身份（§1 三元组；`installation.inspect` 同源）。
2. 从渠道配置的 URL 拉取 release.json（验收用**受控本地 HTTP 服务**充当渠道；
   渠道配置记录在数据根，测试用隔离临时目录）。
3. 校验 manifest：schemaVersion=2、product、tag 正则、`sourceSha` hex40、
   `version`/`date`/`tag` 互洽、assets 非空且逐条通过 §2.3 校验
   （沿用基线 `releaseService.js:35-66` 的整套规则）。
4. 匹配本机 platform/arch 的 asset；`updateAvailable = 渠道最新 buildId ≠ 本地`。
5. **不下载包体**（对齐基线 check 只 `git fetch` ref 不 clone，
   `webUpdateService.js:109`）。

completionResult（合同逐字段）：

```json
{ "currentVersion": "0.1.0",
  "updateAvailable": true,
  "releases": [
    { "releaseId": "v2026.11.18", "version": "0.1.1",
      "platform": "darwin/arm64", "verified": true, "requiresRestart": true } ] }
```

- `verified`：check 阶段指 manifest 校验通过（未取包体）。
  完整 sha256/sha512 包体校验发生在 apply（§3.3），「签名不可信」在 apply 报
  `INTEGRITY_ERROR`；check 自身拉不到/校验不过时 `updateAvailable=false`
  且以 `UPSTREAM_ERROR`（网络）/`INTEGRITY_ERROR`（manifest 不可信）失败。
- `requiresRestart`：恒 true（本产品安装切换需重启生效，§3.1）。
- 无更新：`updateAvailable=false`、`releases=[]`，退出码 0。
  （差异说明：基线 `--check` 无更新时退出码 2，`webUpdateCommand.js:119`；
  新产品按 007 §6.8 退出码合同 0=成功，「无更新」是成功结果。）

### 3.3 installation.apply（安装事务）

input：`releaseId`+`expectedBuildId`（必填）、`restartPolicy`（`defer` 默认 /
`whenIdle`）。errors 重点：`BUILD_CONFLICT`、`PLATFORM_UNSUPPORTED`、
`INTEGRITY_ERROR`、`IO_ERROR`。

阶段与失败语义（acceptance「下载或安装失败保留原可运行版本；新旧二进制不混装、
不覆盖旧CLI入口」）：

| 阶段 | 动作 | 中途失败 effect |
|---|---|---|
| 1 锁 | `mkdir state/update.lock`，EEXIST→`UNAVAILABLE`（lockBusy；对齐基线 `webUpdateService.js:133-138`） | not_applied |
| 2 解析 | 渠道/本地解析 releaseId→manifest；实际 buildId ≠ `expectedBuildId` → `BUILD_CONFLICT`（对齐基线 targetChanged，`webUpdateService.js:141`） | not_applied |
| 3 获取+校验 | 下载（或复用本地缓存/本地保留代际）→ sha256+sha512+size 逐条校验（对齐基线 `updates.ts:46-51` verifyDownloadedFile）→ platform/arch 匹配否则 `PLATFORM_UNSUPPORTED` → `INTEGRITY_ERROR` | not_applied，删部分下载 |
| 4 staging | 解包到 `staging/<tmp>/`，校验代际完整（bin+manifest 齐全、manifest 与包内一致——对齐基线 prepared 校验 `webUpdateService.js:148-150`） | not_applied，删 staging |
| 5 提交 | `rename staging/<tmp> → generations/<buildId>`；重读锁与 current 状态（对齐基线切换前复查 `webUpdateService.js:161`） | not_applied |
| 6 切换 | 原子 `rename` 新 symlink 覆盖 `current` | **applied**（极小窗口；如实报告并提示 restart） |
| 7 收尾 | 写 `state/last-update.json`；裁剪旧代际（保留最近 2 代，被裁代际先确认不是 current）；`recoveryAvailable=true` | applied（收尾失败不回滚切换，错误如实报 `IO_ERROR`） |

- **切换前旧版本原样保留**：不做任何原地改写（对比基线对 dist/node_modules 的
  逐项 backup-rename，`webUpdateService.js:163-175`——新产品以「整代际目录 +
  current 链接」把该序列简化为一次原子 rename，语义等价且窗口更小）。
- `restartPolicy=defer`：安装完成即返回（`restartRequired=true`），由操作者显式
  `installation.restart`。`whenIdle`：安装后挂起等待，直到本产品拥有的服务无
  活跃 run 时自动执行 restart 的 drain 流程；等不到不伪装完成（超时如实
  `DEADLINE_EXCEEDED`）。
- **回滚即 apply**：对已保留旧代际再次 apply（`releaseId` 可解析到本地
  `generations/` 中已存在的 buildId，此时无需网络）。见 §5。

### 3.4 installation.control_update（暂停/恢复/取消）

input：`operationId`+`action∈{pause,resume,cancel}`；execution=sync。
阶段支持矩阵（设计；依据基线桌面更新器状态机 `apps/desktop/src/updates.ts`：
cancel 仅 checking/downloading/paused（:204-212）、pause 仅 downloading
（:213-219）、resume 仅 paused（:220-229）、verification 开始后取消不再提供
（:179-180）；并按合同 acceptance「只在阶段支持时暂停；提交安装阶段不得伪报
可取消；取消不留下半安装」收敛到 apply 的阶段）：

| apply 阶段（§3.3） | pause | resume | cancel |
|---|---|---|---|
| 1–2 锁/解析 | — | — | 支持 |
| 3 获取+校验（下载中） | 支持 | 支持 | 支持 |
| 4 校验已开始 | 否 | — | 否（对齐 updates.ts:179） |
| 5–7 提交/切换/收尾 | 否 | — | 否（合同：「提交安装阶段不得伪报可取消」） |

不支持的动作返回 `INVALID_STATE`；`operationId` 未知返回 `NOT_FOUND`。
cancel 的清理：删除部分下载与 staging；`current` 与既有代际不动——
**无半安装**。

### 3.5 安装时拒绝覆盖已有 pilotdeck（与 shim 衔接）

- 发行包安装器（以及任何把 `bin/pilotdeck` 放进 PATH 的显式步骤）在写入前检查
  目标路径：已存在名为 `pilotdeck` 的可执行文件 → **拒绝**，不覆盖、不移动旧文件
  （shim crate `install.rs` 的同一策略，S4 计划 §4；C1「安装不覆盖已有
  pilotdeck」）。
- 建议错误文案（安装器与 shim 共用）：
  `refusing to install pilotdeck compatibility shim: an existing pilotdeck executable was found at <path>; the shim never overwrites it. Remove it yourself or install the shim under agentcpm-pd's own directory.`
- shim 默认安装位置：`<installRoot>/generations/<buildId>/bin/pilotdeck`
  （不在 PATH 上），是否链接到 PATH 由用户显式决定（§2.5）。

---

## 4. 诊断报告格式

### 4.1 installation.inspect（local、readOnly、caller=local_os_owner）

不经 Gateway（execution=local，007 §5 line 130 的 local 边界）。合同 result
逐字段的取值设计：

| 字段 | 取值 |
|---|---|
| `productName` | `"agentcpm-pd"` |
| `version` | §1 `version` |
| `buildId` | §1 buildId；`--version`（`lib.rs:312-320`）与握手 `cli.build` 必须同源 |
| `platform` | `"<os>/<arch>"`（如 `darwin/arm64`，与 manifest 同词表） |
| `installationMode` | §1 词表 |
| `updateSupported` | 按 §1 表 |
| `reasonCode` | updateSupported=false 时非空（§1 词表）；否则 null |
| `activeUpdateOperationId` | `state/update.lock` 存在时对应进行中操作 id；否则 null |

acceptance 逐条落点：「显示独立安装身份」=上面全部字段只描述 agentcpm-pd 自身；
「旧 pilotdeck 存在不会被覆盖/不影响报告」=inspect 不读写 `~/.pilotdeck`，且
PATH 上已有 pilotdeck 不改变任何字段（最多在人类可读输出附注）；「不把客户端
版本当远端服务版本」=本操作只报本地安装身份，远端服务版本由握手 `target.
gatewayVersion`（007 §6.2 line 174）另报，两者不混写。

### 4.2 installation.diagnose（async）

input：`checks[]∈{installation,service,runtime,channels,mcp,storage}`（默认
前三）+ `includeLogs`。completionResult：`reportArtifactId`、`checks[]`
（name/status/code/message）、`redacted`（恒 true——见 4.4；报告必然脱敏，
该字段向调用方确认这一点）。

**status 词表**（实现层）：`pass` / `warning` / `fail` / `skipped`（未执行，如
域未装配）/ `unsupported`（当前平台或安装模式不适用）。合同 acceptance
「明确检查未执行/不支持，不能全绿占位」→ skipped/unsupported 是一等状态，
绝不映射成 pass。

**checks 枚举 → 具体检查项**（每类含的子检查；子检查 name 采用
`<类>.<项>` 命名）：

| 类 | 子检查（name） | 方法 |
|---|---|---|
| installation | `installation.manifest` | 安装根 current 代际 manifest 完整、SHA256SUMS 复核 |
| | `installation.generations` | 代际目录可读、current 链接有效、last-update.json 可解析 |
| | `installation.targets` | 数据根内客户端目标配置可读、credentialRef 指向的凭据文件存在（不读值）、loopback 地址可达性 |
| service | `service.status` | 本地 `service.status` 语义：读 `<home>/run/ready.json`（`pid/instanceId/generation/protocolVersion/wsUrl/serverRevision`，bootstrap.rs:47-59 已实现结构）+ 进程存活探测 → `stopped/starting/ready/draining/unresponsive` |
| runtime | `runtime.health` | `system.health`（alive/ready/diagnostics） |
| channels | `channels.configured` | `channels.list` 只读；已配置渠道用 `channels.validate`/`channels.probe`（只读探测） |
| mcp | `mcp.configured` | `mcp.list` + 对每项 `mcp.validate` |
| storage | `storage.dataRoot` | 数据根存在、权限 0700（007 §8 line 361）、instance.json 0600 可解析 |
| | `storage.stateDb` | `v1/state.sqlite` 可打开（PRAGMA integrity_check 快速档）、schema 版本 ≤ 本代际支持版本 |
| | `storage.freeSpace` | 数据根所在卷剩余空间低于阈值（建议 <1 GiB）报 warning |

**自举说明**：diagnose 经实例受理（async），Gateway 完全起不来时 CLI 收到
`UNAVAILABLE`——此时指引操作者用 local 组合（`installation inspect`、
`service status`）获取基础诊断；不发明 CLI 侧业务旁路（007 §3「客户端组合」
边界，命令树 `cli-v1-command-tree.json` 中 `installation inspect` 的
lifecycle 注记）。诊断**不自动修复、不安装、不迁移**（acceptance）。

### 4.3 完整报告 artifact（`reportArtifactId` 引用的内容）

mediaType `application/json`，存为合同 artifact（`$defs.artifact`：
artifactId/length/sha256/mediaType）。结构：

```json
{
  "schemaVersion": "agentcpm-pd.diagnose-report/1",
  "capturedAt": "2026-11-18T00:00:00Z",
  "identity": { "productName": "agentcpm-pd", "version": "0.1.0",
                "buildId": "0.1.0+a1b2c3d4e5f6.x1y2z3w4q5r6",
                "sourceSha": "<40>", "contractDigest": "<12>", "tagName": "v2026.11.18" },
  "platform": { "os": "darwin", "arch": "arm64", "osVersion": "…",
                "installationMode": "managed" },
  "gateway": { "serviceState": "ready",
               "ready": { "pid": 123, "instanceId": "…", "generation": "…",
                          "serverRevision": "…", "wsUrl": "…" } },
  "storage": { "dataRoot": "~/.agentcpm-pd", "mode": "0700",
               "stateDb": "ok", "schemaVersion": 1, "freeBytes": 12345678 },
  "targets": [ { "name": "local", "health": "ok",
                 "credentialRef": "credentials/local.token",
                 "loopbackReachable": true } ],
  "recentErrors": [ { "runId": "local-…", "state": "interrupted",
                      "code": "…", "at": "…" } ],
  "checks": [ { "name": "installation.manifest", "status": "pass",
                "code": null, "message": "…" },
              { "name": "service.status", "status": "fail",
                "code": "GATEWAY_UNRESPONSIVE", "message": "…" } ],
  "redaction": { "applied": true,
                 "rules": ["credential-values", "secret-env", "home-paths",
                           "log-excerpt-truncation"] }
}
```

- `recentErrors`：v1 库中最近 failed/interrupted/result_unknown 的 run 与
  operation 摘要（默认各 ≤10 条，`includeLogs=false` 不含日志正文）。
- `identity` 同时含 version/buildId/sourceSha/contractDigest/tagName——
  「保存版本身份」证据直接取此对象。

### 4.4 脱敏规则（`redacted=true` 的含义）

对齐 `consumer.req.029`（「日志可按 request/run 关联但不包含凭据/原始敏感正文；
导出诊断说明脱敏范围」）与 diagnose acceptance「日志和路径脱敏」：

1. 凭据**值**不进报告；只出现 `credentialRef` 引用名。
2. 环境变量中 `*TOKEN`/`*KEY`/`*SECRET`/`*PASSWORD` 后缀键的值 → `<redacted>`。
3. home 绝对路径 → `~` 前缀缩写；安装根外其他用户路径同样缩写。
4. `includeLogs=true` 时：日志片段先过 1–3 管道，再按源截断（每源默认 100 行），
   并标注截断。
5. `recentErrors[].message` 复用同一管道。
6. 报告 `redaction.rules` 列出实际套用的规则——「导出诊断说明脱敏范围」。

---

## 5. 回滚步骤

### 5.1 回滚机制：apply 旧代际 + restart

合同没有独立 rollback 操作，**不发明**。回滚 = 对已保留旧代际再次
`installation.apply`（releaseId 解析到本地 `generations/<旧buildId>`，无需网络）
+ `installation.restart`。`recoveryAvailable=true`（apply completionResult）
即表示旧代际仍在保留窗口内。

与基线「禁止自动降级」（`webUpdateCommand.js:19` reasons.ahead
"Automatic downgrades are disabled"、`webUpdateService.js:114`）的差异：
基线禁止的是 **check→apply 自动链路**中的降级；新产品把**显式**回滚定义为
apply 的合法用法（操作者指定旧 releaseId + expectedBuildId），不经过
updateAvailable 判定。这是已接受差异候选，需登记（§10.1）。

### 5.2 人读回滚步骤（产品文档口径）

1. **确认动机**：新版不可用/回归。先 `installation diagnose` 留档（含
   recentErrors）。
2. **数据兼容预检**（关键）：回滚 apply 的阶段 2 之前增加数据格式检查——
   读 `v1/state.sqlite` 的 schema 版本，若高于旧代际二进制支持的 schema 版本
   → **拒绝回滚**（`INVALID_STATE`，message 说明「新代已写入更高数据格式，
   回滚会丢数据；请前滚修复或用备份恢复」，并指向 t020 的备份/恢复工具）。
   依据：合同 acceptance「回滚前检查数据格式兼容，不为回滚偷偷降级或丢新数据」。
3. `installation apply --release-id <旧tag> --expected-build-id <旧buildId>`。
4. `installation restart --param-target ownedService --expected-build-id <旧buildId>`。
5. 验证（回滚的验证步骤）：
   - `installation inspect` → buildId 恢复为旧值；
   - `system.health` → alive+ready；ready.json 的 `serverRevision`/`generation`
     与旧代际一致；
   - `run list --states queued,running,waiting,cancelling`（D4/D13 口径）核对
     恢复扫描结果；
   - 抽查一条业务命令（如 `run submit` 冒烟）退出码 0。

### 5.3 回滚时正在运行的后台任务 / runs

- apply（文件层切换）**不影响**运行中的进程；只有 restart 才中断。
- restart 的 drain：拒新提交、等待活跃 run 终止，`drainTimeoutMs` 默认 30000
  （合同）；超时**如实可见**（`DEADLINE_EXCEEDED`，绝不伪报已停）。操作者想
  立即回滚需先显式 `run.cancel`。
- 重启后旧代 Gateway 启动，按 007 §7.5（line 342-349）恢复：`running/cancelling`
  → `interrupted`（无法确定副作用 → `result_unknown`）；`queued` 保留但队列
  `pauseReason=gateway_restart` 暂停；待处理交互 → `expired`；
  **不自动重放**（D9）。续接由操作者显式进行（t009 的 `run.continue`/steer 家族，
  台账 `cli.task.t009`）。

### 5.4 旧版本数据目录兼容（「v1 SQLite 只前读？」）

结论：**不是只前读，而是「检测到更高 schema 时拒绝写路径、尽力只读服务」**：

- v1 库记录自身 schema 版本（PRAGMA user_version 或 meta 表）；每个代际二进制
  带 `SUPPORTED_V1_SCHEMA` 上限。
- 旧代际读库：schema ≤ 支持上限 → 正常读写；schema > 上限 → 写操作一律
  `INVALID_STATE`（message 指向前滚或备份恢复），只读操作尽力服务并在诊断
  `storage.stateDb` 项标注 `warning`（code 如 `SCHEMA_NEWER_THAN_BINARY`）。
- 该口径是设计决定（合同只要求「检查兼容」），SQLite 跨 schema 读的语义风险
  在验收中专门测（§8），**待产品验证**。

---

## 6. 与 compat shim 的衔接

### 6.1 发行包中的位置与版本对应

- shim 二进制与主二进制出自**同一源码、同一次构建**：manifest 的
  `buildId`/`sourceSha`/`contractDigest` 同时约束两者；`compatManifestDigest`
  固定 shim 行为清单的指纹。
- `compat-manifest.json`：27 入口 → 处置（支持/需差异/拒绝）→ 目标操作序列
  （S4 计划 §2 的 27 行表；crate 内 `manifest.rs` 只读可验证）。
- **C1 主版本对应（D15 在发行层的体现）**：
  - 构建期：发行包校验 shim 的合同版本主版本段与 agentcpm-pd 的
    `api_version` 主版本一致，不一致构建失败（发行门禁）。
  - 运行期：shim 子进程调 agentcpm-pd 公开入口握手时，主版本不一致 →
    -32006 拒绝（007 §6.2 line 176-178），shim 如实透传该错误并提示
    「shim 与 agentcpm-pd 主版本不匹配」。
  - 发行包混装防护：apply 的阶段 4 校验 staging 中两个二进制的 buildId
    前缀一致（同 `<version>+<sourceSha12>`），不一致 `INTEGRITY_ERROR`
    ——「新旧二进制不混装」的发行层执行点。
- 用户可能单独升级 agentcpm-pd 而旧 shim 仍在：运行期 -32006 会拦住；发行文档
  说明 shim 跟随主产品同包升级。

### 6.2 安装拒绝策略（与 shim crate install.rs 对齐）

- 发行包安装器放置 `bin/pilotdeck` 前的检查与文案见 §3.5；两处共用同一实现
  （crate 的 install 模块被安装器复用，避免两套判断）。
- 安装器不建 PATH 链接、不写 shell rc 文件（§2.5）。

---

## 7. 升级与 shutdown（cli.contract.shutdown-upgrade）

### 7.1 合同语义到发行层

| 合同要求（acceptanceCriteria / case 步骤） | 本文落点 |
|---|---|
| active run/queued/审批等待/PTY 各自停服策略 | restart drain 只拒新等活跃；审批待办持久化（007 §7.4）不因重启丢失（变 expired 的按 §7.5）；PTY 归 t015 域，restart 前由操作者处理或按 PTY 合同销毁 |
| 取消超时保留状态 | `drainTimeoutMs` 超时 → `DEADLINE_EXCEEDED`，服务与 run 状态原样保留，不伪报已停 |
| 启动旧代服务后更新 current，新 worker 不混入另一代 | 代际目录 + current 原子切换（§3.1）：旧代进程持有的二进制 inode 不变，新 worker 由 restart 后按 current 启动——**按代际隔离，不混装** |
| 重启选定新代 | `restart.expectedBuildId` = 新 buildId；启动后核对 ready.json `serverRevision` 与之一致才 `healthy=true`（`HEALTH_CHECK_FAILED` 否则） |
| 只重启本产品拥有的进程 | `target=ownedService`；越权 → `RESOURCE_NOT_OWNED`；`currentHost`（桌面宿主）在 CLI 场景 → `UNAVAILABLE`（acceptance 原文） |
| D13 桌面关闭询问 | 桌面宿主行为（弹窗「保留后台运行」默认/「全部取消并停止服务」），CLI 无关，本文不实施 |

### 7.2 更新期间幂等键与未决写（007 §6.6 对齐）

- `installation.check/apply/restart` 均 `idempotentSubmit`：argv/`api invoke`
  入口由 CLI 生成并持久化幂等键（007 §6.3 line 198）；`api connect` 上由调用方
  自备。apply 重试（同键同输入）回放原回执，不二次切换。
- **服务重启造成的在途写**：restart 期间 Gateway 连接断开——在途写请求回复
  `RESULT_UNKNOWN`（effect=unknown）并回显幂等键；订阅以 `disconnected` 结束
  附 `resumeCursor`（007 §6.6 line 250-253、T9）。调用方/宿主在重连后先用
  `requests.lookup {methodId, idempotencyKey}` 对账，确认后才放行同一意图的
  新写入（007 §6.6 line 257）。
- apply 操作本身被中断（进程被杀）：锁目录残留 → 下次 apply `lockBusy`
  （`UNAVAILABLE`），由 `installation diagnose` 的 `installation.generations`
  项指出残留锁；操作者确认无进行中更新后清锁（诊断不自动清理——「诊断不自动
  修复」）。staging 残留同理，检查项报 `warning` 并给出人工清理指引。
  （对齐基线「lockBusy：另一个更新或未确认的清理持有锁」
  `webUpdateService.js:21`。）

---

## 8. 用例映射与验收证据

### 8.1 流程 → 台账用例映射

| 本文章节/步骤 | 用例 id |
|---|---|
| §1 版本身份、`--version`、握手 build | `cli.case.operation.installation.inspect`；`cli.case.c034`（机器输出） |
| §2 发行包与干净安装（双平台） | `cli.case.c035`（双平台发行）；`cli.case.requirement.backend-kernel.cli.req.name`（二进制共存/home 隔离/升级回滚验证） |
| §3.2 check（网络失败/签名不可信/无更新分别报告） | `cli.case.operation.installation.check` |
| §3.3 apply（校验/原子切换/失败保留原版本/不混装） | `cli.case.operation.installation.apply`；`cli.case.requirement.compatibility.cli.requirement.installation-update`（失败保留可用版本、取消只杀 owned updater、无静默重启） |
| §3.4 control_update（阶段矩阵、取消无半安装） | `cli.case.operation.installation.control_update`；`cli.case.requirement.consumers.consumer.req.022`（耗时操作状态可 headless 操作） |
| §3.5 + §6 拒绝覆盖 pilotdeck、shim 隔离 | `cli.case.contract.capability.a040` 第 3 步（C1 隔离边界）；`cli.case.requirement.backend-kernel.cli.req.compat` |
| §3.3/§5.3 restart（drain/超时/健康检查/ownedService/currentHost） | `cli.case.operation.installation.restart`；`cli.case.requirement.consumers.consumer.req.024` |
| §4 诊断（两平台实际结果、不自动修复、脱敏、不全绿） | `cli.case.operation.installation.diagnose`；`cli.case.requirement.consumers.consumer.req.029`；`cli.case.requirement.consumers.consumer.req.025`（跨平台真实组合、产物 hash、能力矩阵应测/实测/跳过/不支持） |
| §5 回滚（数据兼容预检、interrupted 语义） | `cli.case.contract.contract.shutdown-upgrade` 第 2/3 条；`cli.case.c032`/`cli.case.c033`（迁移与中断恢复，t020 联合） |
| §7 升级与 shutdown | `cli.case.contract.contract.shutdown-upgrade` 全部三条 |

### 8.2 「保存版本身份」的证据格式建议

每条验收保存一个 evidence JSON（对齐用例「证据要求」字段：
argv/stdin、schema、stdout/stderr/退出码、前后状态、源码/产物 hash、平台、
时间、环境与依赖身份、秘密脱敏）：

```json
{
  "capturedAt": "2026-11-18T00:00:00Z",
  "platform": "darwin/arm64",
  "identity": { "version": "0.1.0", "buildId": "0.1.0+…",
                "sourceSha": "<40>", "contractDigest": "<12>" },
  "artifacts": { "archiveSha256": "<发行包 sha256>",
                 "manifestSha256": "<release.json sha256>",
                 "binarySha256": "<bin/agentcpm-pd sha256>" },
  "argv": ["agentcpm-pd", "installation", "diagnose", "--checks", "…"],
  "stdin": null,
  "exitCode": 0,
  "stdout": "<原文>", "stderr": "<原文>",
  "before": { "buildId": "…", "generations": ["…"] },
  "after":  { "buildId": "…", "generations": ["…"] },
  "environment": { "AGENTCPM_PD_HOME": "<隔离临时目录>", "channelUrl": "<受控渠道>" },
  "dependencies": { "gatewayBuildId": "…", "shimBuildId": "…" }
}
```

- 双平台各存一套（本机/mbp2 macOS + cb8 Linux），文件名
  `evidence-<case>-<platform>-<date>.json`，放阶段归档目录（不入库的 `_tmp/`
  过程区 + 台账登记的摘要行）。
- 失败路径同样保存（「失败保留原件」）。

### 8.3 验收环境

- `AGENTCPM_PD_HOME` 一律指向隔离临时目录（用例前置条件；AGENTS.md 不碰
  `~/.pilotdeck`/`~/.agentcpm-pd`）。
- 渠道用受控本地 HTTP 服务提供 release.json + 资产；故障注入：坏 sha256、
  坏 manifest 字段、平台不匹配、断网（check 的三种失败报告）。
- 回滚数据兼容用例：手工把 v1 库 schema 版本调高再回滚，断言 `INVALID_STATE`。

---

## 9. 实施依赖

### 9.1 台账进入条件（正本）

`cli.task.t021` 前置：`cli.task.wp0-gate`、`cli.task.t009`、`cli.task.t015`、
`cli.task.t020`。含义：

| 前置 | t021 用到什么 |
|---|---|
| wp0-gate（S2 集成门） | 28 操作端到端与公共 receipt/operation 机制（apply/diagnose 以 operation 形态受理） |
| t009（run.continue/steer.*） | 回滚后 interrupted run 的显式续接（§5.3） |
| t015（terminals.*） | shutdown 合同「PTY 各自停服策略」验收项（§7.1） |
| t020（data.*/migration.*） | 回滚数据兼容预检（§5.2 第 2 步）与备份恢复指引（§5.4） |

注意：t015 台账状态「未开始」（`cli-v1-design.json` tasks）。t021 实施排期
受它约束；若 t015 滞后，PTY 相关验收项按用例「具体不适用项须说明」如实标注
延后，不降低其余门槛。

### 9.2 S3 依赖（按域）

- **公共操作**：`operations.get`（control_update 的 operationId 查询与
  activeUpdateOperationId 对账）、`requests.lookup`（§7.2 对账）、
  `system.health`、`instance.inspect_initialization`、`service.status/
  start/stop`、`run.list`（回滚验证）、`mcp.list/get/validate`、
  `channels.list/validate/probe`（诊断 checks）。
- **CLI 外壳**：`installation {inspect,check,apply,control-update,restart,
  diagnose}` 命令条目已在命令树登记（`state/cli-v1-command-tree.json` 的
  installation 段），argv 渲染由 S3 集成者的 CLI 层提供；本任务交付领域适配
  （local execution 部分：安装根、锁、代际、manifest、渠道客户端、报告生成）。
- **注册表**：`installation.*` 的 local/async 执行类型与 priority 按合同
  （inspect=local/readOnly，其余 async/idempotentSubmit，control_update=
  sync/idempotentSubmit）。

### 9.3 构建与验证分工（007 §11.3 line 493）

- 不依赖 Gateway 的部分（manifest/校验/安装布局库、诊断报告生成）本机构建；
- 发行包全量构建与跨平台集成在 `cb8`（Linux，`CARGO_BUILD_JOBS=6`，源码
  `git bundle` 传送，Kernel 固定 `fb54c74a`）与 `mbp2`/本机 macOS 并行；
- 每平台跑四条命令全量（fmt/clippy/nextest/doc）；
- 产物与 evidence 的 hash 双平台分别记录（req.025「记录实际产物hash」）。

---

## 10. 未决问题（需用户/集成者裁决）

1. **显式回滚 = apply(旧 releaseId)**：合同无 rollback 操作，本设计以显式
   apply 承担回滚（与基线「禁止自动降级」的差异见 §5.1）。请确认登记为已
   接受差异，或要求新增合同操作。
2. **manifest schemaVersion=2**：不复用基线 release.json 的 1（字段集不同，
   旧更新器按 ===1 校验不会误读）。请确认独立渠道口径。
3. **签名机制**：当前 `verified` = sha256+sha512+manifest 一致性校验（沿用
   基线强度，`updates.ts:46-51` + `releaseService.js:47-66`）；是否追加
   内容签名（如 ed25519/minisign）待裁决——基线无此机制，加了属新功能。
4. **渠道服务形态**：产品渠道（URL、托管、stable/preview 的定义）未定；
   验收先用受控本地 HTTP。preview 渠道在基线无对应物（`allowPrerelease=false`
   `updates.ts:74`），若启用需定义。
5. **旧代际对高 schema 库「拒写、尽力只读」**（§5.4）：设计决定，待产品
   验证与集成者确认口径。
6. **installationMode/reasonCode 词表**（§1）：实现层词表需与 S3 集成者在
   注册表登记时统一，避免两套叫法。
7. **t015 时序**：PTY 停服策略验收依赖 t015（未开始），t021 验收中该项可能
   需按「不适用项说明」延后——需用户接受该排期风险。
8. **系统级安装（/usr/local、deb/rpm）**：不在本轮；OS 包格式发行是否开
   后续任务待裁决（基线桌面有 deb/rpm 流程，desktop-linux.yml:25-37）。

---

## 附录 A：基线行为事实索引（均为只读核对，resources/PilotDeck/ @ cc7c4e6）

| 事实 | 位置 |
|---|---|
| `pilotdeck update` 只接受 `--check`/`--restart` | `scripts/update-web.mjs:8` |
| check→apply→restart 的 CLI 编排；upToDate 退出码 2 | `ui/server/services/webUpdateCommand.js:116-131`（119 退出码 2、123 restartUnavailable、129 prepared 文案） |
| apply 为 NDJSON 进度流，断线后按 updateId 轮询 status 对账 | `webUpdateCommand.js:72-107`（77 一小时上限、96-106 轮询） |
| 更新锁 = `.git/pilotdeck-update.lock`（mkdir），EEXIST→lockBusy | `ui/server/services/webUpdateService.js:133-138`、`:72` |
| staging = `.git/pilotdeck-update-<tmp>`；`clone --shared --no-checkout` + `checkout --detach` | `webUpdateService.js:143-146` |
| prepared 构建校验（clean、sourceSha 一致、构建不改源、artifact 齐全） | `webUpdateService.js:148-153` |
| 切换前复查工作区；逐 artifact backup-rename→install→失败反向恢复，恢复失败保留备份（manualRecovery） | `webUpdateService.js:161-192` |
| 切换 = `git merge --ff-only`；成功后 `needsRestart:true` | `webUpdateService.js:180`、`:193` |
| 失败保留 staging 的条件（processStopFailed）与 lastUpdateResult 记录 | `webUpdateService.js:195-209` |
| 自更新前置拒绝集合（development/localChanges/container/desktop/notGit/unofficial/shallow/unknownVersion/runtimeMismatch/ahead/diverged/lockBusy/restartRequired） | `webUpdateService.js:9-23`、检查在 `:64-94` |
| check 结果形状（source/mode/hasUpdate/canUpdate/reason/current/latest） | `webUpdateService.js:96-119` |
| release manifest 来源 `releases/latest/download/release.json` 与字段校验 | `ui/server/services/releaseService.js:35-66`（tag 正则 :4、sha 校验 :56-66） |
| 桌面更新状态机与 pause/resume/cancel 阶段限制 | `apps/desktop/src/updates.ts:9-17`（状态）、`:204-229`（cancel/pause/resume）、`:179-185`（verification 后不可取消） |
| 下载校验 = size+sha256（sha512 由 electron-updater feed 校验） | `updates.ts:46-51`、`:34-44` |
| updater 配置：autoDownload/autoInstallOnAppQuit/allowDowngrade 均 false | `updates.ts:70-76` |
| restart 路由三模式（docker/supervisor/direct spawn detached），202 accepted + previous 身份 | `ui/server/routes/update.js:28-106`（57-68、72-95） |
| apply 路由 NDJSON（stage=progress/complete/error + timestamp） | `ui/server/routes/webUpdate.js:12-35` |
| 发行 manifest 生成（release.json + SHA256SUMS.txt；platform/arch 归一化） | `apps/desktop/scripts/create-release-manifest.mjs:20-40`、`:59-67` |
| 构建身份 web-build.json（sourceSha/tagName/clean/buildTime） | `scripts/write-web-build-metadata.mjs`（全文 20 行） |
| 桌面发行平台矩阵：macOS arm64/x64、Linux deb x64/arm64 | `.github/workflows/desktop-macos.yml:48-57`、`desktop-linux.yml:29-37` |

新产品侧已交付依据：`--version` 输出（`src/agentcpm-pd-cli/src/lib.rs:312-320`）、
`--version` 解析（`src/agentcpm-pd-cli/src/commands.rs:210-211`）、ready.json
结构（`src/pilotdeck-gateway/src/public_v1/bootstrap.rs:47-59`）、命令树
installation 段（`state/cli-v1-command-tree.json`）。
