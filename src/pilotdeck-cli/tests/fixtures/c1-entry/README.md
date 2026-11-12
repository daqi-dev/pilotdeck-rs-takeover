# C1 正式 CLI 入口验证

本目录只驱动真正的 `pilotdeck` 进程，不导入产品函数。固定 TypeScript 提交为
`cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。

## 已捕获的 setup 基线

`cases.json` 定义五个无网络场景：WeCom 手工配置允许名单、空允许名单降为
disabled、已有 WeCom 凭据重用、已有 Weixin 凭据重用、手工问答中途 EOF。
`capture.mjs` 等 stdout 中对应提示出现后才发送下一行，避免一次性 stdin 使基线
readline 丢掉后续答案。每个子进程使用独立 HOME/PILOT_HOME/配置目录，10 秒上限。
仅收尾本次创建的进程组。

2026-11-13 在 macOS ARM64、Node 22.22.0 上，真实 TS 入口两次各 5/5 通过；
仅将 Node ExperimentalWarning 的 PID 归一后，两次输出与全部落盘文件相同。
原始证据在 C1 worktree 的 `_tmp/20261113-c1-entry/setup-ts-{a,b}/`。
`source.json` 保存固定源码、构建入口、采集器、用例与 oracle 哈希。
`oracle.json` 的 stdout 和落盘文件不做内容归一。

```sh
node --test src/pilotdeck-cli/tests/fixtures/c1-entry/capture.test.mjs
node src/pilotdeck-cli/tests/fixtures/c1-entry/capture.mjs ts /absolute/baseline NEW_OUTPUT_DIR
node src/pilotdeck-cli/tests/fixtures/c1-entry/capture.mjs rust /absolute/pilotdeck NEW_OUTPUT_DIR
```

采集器自测覆盖缺输出、错误退出、信号退出、缺提示、缺失/错误落盘、UTF-8 跨块和
真实子进程超时；3/3 已通过。该成绩只说明采集器可用，不是 Rust 产品通过。

`c1_entry_setup.rs` 在 Cargo 构建的正式二进制上复现相同逐提示输入并逐字比较
stdout 和完整配置/凭据文件树；它不需要 Node 或参考 checkout。测试本身始终
隔离 HOME/PILOT_HOME，未匹配的多余文件也会失败。Rust 编译与执行由集中队列完成：

```sh
cd src
cargo test -p pilotdeck-cli --test c1_entry_setup
```

## bbx 场景与 E0 命令

新增场景：

- `c1-setup-existing-success`：两个正式 setup 重用凭据写配置入口，7 项正向断言。
- `c1-sdk-roundtrip`：run/model → resume → sessions messages，provider 同时要求两轮
  输入以验证历史确实传给模型；另含 provider 错误，14 项正向断言。
- `c1-sdk-cancel-resume`：模型先输出再挂住，向本次 CLI 发 SIGINT 后续跑同一会话，
  11 项正向断言。退出形状尚待固定基线执行确认，不提前声明通过。

三场景均已通过 JSON/断言定义检查；给空产物时 32 项必需断言全部失败。
完整 bbx 尚待 E0 在固定集成提交上先 TS 自比再 Rust 对拍，不能把场景存在当作成绩。
在 cb8 私有 Docker 网络中，使用 E0 的固定 Node 22.22.0、TS 构建与 Rust 候选：

```sh
scripts/backend-validation/bbx-docker.sh /home/clawbox-08/ws/pilotdeck-rs-cc2c NEW_SELF_DIR --self c1-setup-existing-success c1-sdk-roundtrip c1-sdk-cancel-resume
scripts/backend-validation/bbx-docker.sh /home/clawbox-08/ws/pilotdeck-rs-cc2c NEW_PARITY_DIR c1-setup-existing-success c1-sdk-roundtrip c1-sdk-cancel-resume
```

这些新增场景不关闭 CLI 总范围：cron/默认在线入口依赖固定 18789 的 Docker 验证；
三渠道网络 setup 仍需私网 DNS/TLS mock 和逐提示输入，现有 bbx 的单块 stdin
不能证明那些成功路径；update 的 Git Web 管理组件属于明确保留的可选外部组件。

## update 保留组件观测

`update-capture.mjs` 启动固定 TS 的正式 `pilotdeck update`，以随机 loopback 端口
提供 Web `/health`、`/api/update/{context,check,apply,restart}` mock。临时 SQLite
里只有本次的假用户和签名密钥，mock 验证 JWT 签名与身份；没有修改基线安装。

```sh
node src/pilotdeck-cli/tests/fixtures/c1-entry/update-capture.mjs /absolute/baseline NEW_OUTPUT_DIR
```

两轮各 9/9 已执行：check、upToDate（外部 updater code 2 映射 CLI code 0）、apply、
restart、apply 失败，以及 SIGINT/SIGTERM 分别发给 CLI PID 和整个 owned 进程组。
此处 PASS 表示采集和对应正常/失败预期均完成，不表示取消行为符合修复后标准。
原始证据在 `_tmp/20261113-c1-entry/update-ts-{a,b}/`。将 Node 警告 PID、进程 PID
与 updateId UUID 归一后，9 对完整结果均 SAME；请求方法、路径、请求体、假用户
JWT 签名验证、输出、退出信号与取消后连接状态全部保留。

已观察到基线缺陷：对 CLI 父 PID 发 SIGINT/SIGTERM，父进程以该信号退出 200ms 后，
本次进程组仍存在且 mock apply HTTP 连接仍打开；对整个本次进程组发信号，两者均
关闭。采集器最后只清理自己创建的进程组。该差异已交给协调者登记与 updater 作者
修复；不能将父进程结束误报成更新子进程已停止。

## 三渠道 HTTPS 正式入口（待 E0 执行）

`setup-https-capture.mjs` 通过真实供应商 URL 驱动正式 `gateway setup`。它要求
Linux Docker、仅 loopback 网卡和四个域名只解析到 127.0.0.1；任何条件不符先
失败，尚未创建产品子进程。进程内 TLS mock 使用临时自签证书，产品只收到标准
`NODE_EXTRA_CA_CERTS`，没有产品测试专用网络开关或函数替换。

八例包括飞书校验成功、Lark 拒绝后取消、飞书拒绝后仍保存、WeCom 扫码成功、
Weixin 扫码成功、非默认 PILOT_HOME 不存在、写 YAML 失败后凭据保留、扫码 HTTP
失败。请求方法/域名/路径/次数及飞书请求体都有断言；原始请求头、输出和落盘保存。
非默认 PILOT_HOME 的 TS 失败/Rust 成功仅按已登记的
`D1-SETUP-WEIXIN-HOME-001` 精确单格复核；本机尚未实际执行网络场景，不能将其写成通过。

E0 可在现有隔离容器参数上增加以下参数，并将最终 Node 命令换为本脚本。不得
映射宿主端口，基线与源码任务目录按 E0 的既定路径挂载；OpenSSL 使用已安装源码
构建/系统工具，不能下载预编译工具：

```sh
--network none --sysctl net.ipv4.ip_unprivileged_port_start=0 \
--add-host open.feishu.cn:127.0.0.1 \
--add-host open.larksuite.com:127.0.0.1 \
--add-host work.weixin.qq.com:127.0.0.1 \
--add-host ilinkai.weixin.qq.com:127.0.0.1 \
--mount type=bind,src=/usr/bin/openssl,dst=/usr/local/bin/openssl,readonly
```

容器内命令（每次输出目录必须不存在）：

```sh
node src/pilotdeck-cli/tests/fixtures/c1-entry/setup-https-capture.mjs \
  ts /home/clawbox-08/ws/pilotdeck-rs-cc2c/baseline/PilotDeck NEW_TS_OUTPUT --complete
node src/pilotdeck-cli/tests/fixtures/c1-entry/setup-https-capture.mjs \
  rust /home/clawbox-08/ws/pilotdeck-rs-cc2c/checkout/src/target/debug/pilotdeck NEW_RUST_OUTPUT \
  --complete --rust-root /home/clawbox-08/ws/pilotdeck-rs-cc2c/checkout \
  --rust-sha FULL_40_CHARACTER_CANDIDATE_SHA
```

产物路径中的单次输出根由采集参数明确给出。做自比时只把这个根替换为固定标记、
归一 Node 警告 PID；不忽略 YAML、凭据内容、错误信息或请求。首次失败保留并归因。

`--complete` 是完整证据模式：强制 Node 22.22.0、固定 TS 完整 SHA、干净 tracked
源码，记录 Node 可执行文件哈希、TS package/lock 与入口/setup 的 source/dist 哈希、
driver/helper 哈希；Rust 必须显式提供源码根与完整 SHA，并记录实际二进制哈希。
二进制由该提交构建的关系仍由 E0 同次构建记录证明。旧三位置参数调用仍可用，但明确
标记 legacy；即使采到字节也不自动提升成完整身份的产品验收。

每格保存 `input.json`、`command.json`、原始 `stdout.bin`/`stderr.bin` 及哈希，逐请求
`request-N.body.bin`、`rawHeaders`（保留大小写、顺序及重复项）、HTTP 版本与解析后 body。
这是精确 HTTP 应用头/请求体证据，不声称捕获了 TLS 记录、HTTP chunk framing 或原始
请求行。`files-before.json` / `files-after.json` 记录 home/config/project 的每个目录、
文件大小与哈希；`case-evidence.json` 把这些产物绑定到 `identity.json` 的哈希，
`summary.json` 再绑定完整八格清单。比较器重新读取实际文件树和原始字节核验，漂移
一律失败。缺身份的旧产物仍只能提供语义结论，不倒填来源。

用 [compare-README.md](compare-README.md) 的三个只读比较入口完成自比/对拍。
HTTPS 专用证据 helper 的负控无需 Docker 或产品：

```sh
node --test src/pilotdeck-cli/tests/fixtures/c1-entry/https-evidence.test.mjs
```
