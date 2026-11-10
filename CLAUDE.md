# PilotDeck-RS 协作说明

> `AGENTS.md` 是指向本文件的软链接。
> 2026-11-10 起，本分支由一个会话直接负责，向人直接汇报；本文件替换了上一套多模型流水线的说明。
> 旧版用 `git show de356427e:CLAUDE.md` 查看，它是待核实的历史记录，不是当前指令。
> 远端 `main` 上仍有其他 agent 按旧说明工作，所以本文件只留在接手分支上，不要合回 `main`。

## 方向

把 PilotDeck 后端从 TypeScript 重写为 Rust，固定基线是 `OpenBMB/PilotDeck@cc7c4e6`。范围、阶段划分和验收口径的唯一正本是 `.42cog/intent.md`，开工先读它。产品代码在 `src/`（Cargo workspace）。

默认产出是产品代码和对着基线的行为测试。协作工具、流程文档、测试装置本身不是目标，只在挡住产品开发时才动。

## 什么算事实

冲突时从高到低：

1. 基线 `cc7c4e6` 在对应入口实际跑出来的行为
2. 基线的测试
3. 基线的源码（`resources/PilotDeck/`，结论带文件和行号）
4. 基线的文档

Rust 侧现有的代码、测试和夹具都排在这四者之后。

下面这些是上一轮流水线留下的记录，只当线索，用之前对照代码、git 历史和实际运行结果核实：`state/` 下的 board、changelog、决策卡和 memory；GitHub 上的 issue 与 PR 评审块；`specs/product-coverage/entries.json` 的状态字段；标着「AI 代批，待人复核」的任何决定。

## 行为约束

- 对外可观察的行为以基线为准：gateway method 与协议、HTTP / WS / SSE 路由与鉴权、CLI 的参数、输出、退出码、落盘格式、发给模型 provider 的请求。
- 不自己发明协议、字段或错误格式。拿不准就把基线跑一次看输出，不猜。
- 迁移期间不加功能、不顺手改行为。例外只有基线自身的 bug：崩溃、挂起、丢数据；与它自己的类型、文档或测试矛盾；安全问题。修了要登记为已接受差异。
- 不追上游新提交。
- Rust 侧的内部结构、命名、crate 边界可以按 Rust 习惯改，不照搬 TS 的类图。

## 不要动

- `vault/raw/`、`notes/`、`resources/`、任何 `hi.md`：只读。`resources/` 里的内容读懂后自己写，不复制粘贴。
- `~/.pilotdeck`、本机正在运行的 PilotDeck、`~/ws/PilotDeck-rust-program-20261009/`：不读、不写、不启停。测试需要 home 目录时用隔离的临时目录。
- `.42cog/intent.md` 的收敛方向：不改，认为有问题就提出来。
- 冻结的判分文件：`specs/parity/README.md` 与 `FROZEN.md`，以及 `specs/{parity,gateway-parity,ui-server-parity}/accepted-diffs.md`。差异账本只追加，新条目只在对应的 `diffs/` 目录新增文件，规则见各目录 README，由各自的 `scripts/*/check_frozen.py` 检查。不为了让测试变绿去改标准。
- 密钥只放 `.env.local`，不入库；隐私数据不入库；推送前跑 gitleaks。
- 不下载预编译二进制；不强推 `main`；不在 `zhengdaqi` 账号下新建 `pilotdeck-rs`。
- 推送、合并、关闭 issue、删除远端资源这类对外或不可逆的动作，按人在当前会话里给的授权办，没有就先问。

## 构建与测试

在 `src/` 目录下，工具链由 `rust-toolchain.toml` 锁定：

```
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo nextest run --workspace --no-fail-fast --profile ci --config-file ../.config/nextest.toml
cargo test --workspace --doc
```

全量构建又慢又占磁盘，日常先跑受影响 crate 的子集，提交前再跑全量。构建产物放在 `src/target/` 或 `_build/` 下，两处都不入库。

没有行为测试覆盖的模块，先对着基线的实际行为补特征测试，再改。现有的对拍装置在 `scripts/{parity,gateway-parity,cli-parity,ui-server-parity}`，用之前先确认它能抓到一个故意注入的错误。

## 工作方式

- 小步提交，每个提交保持上面四条命令通过。
- 测试没过就说没过并附输出；没核实的就说没核实；没做完的不标完成。
- 改变对外行为、做架构取舍、动「不要动」里的东西之前，停下来问人，给出选项、建议和各自的后果。其余自行决定，汇报时说明依据。
- 停用旧的脚本、闸门或装置时，写明停了什么、它原本想防什么、现在由什么替代。先停用，确认无影响再删。
- 关闭一个模块或阶段之前，grep `unimplemented!`、`todo!`、`NOT_IMPLEMENTED`、空 match 分支和转发给 TS 的兜底，逐条说明是否在范围内。

## 约定

- 沟通和文档用中文，文件名用英文。
- 提交信息：前缀 `feat` / `fix` / `docs` / `chore` / `test` / `build`，后接一句话说清改了什么；尾注的 `Co-Authored-By` 只写实际改了代码的一方。
- 一次性的过程材料放 `_tmp/YYYYMMDD-<名字>/`，不入库。
- 目录分工见 `README.md`，命名规则见 `specs/README.md`。
