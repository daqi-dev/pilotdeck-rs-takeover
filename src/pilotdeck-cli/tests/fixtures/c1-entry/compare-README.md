# C1 正式采集产物的只读比较

以下入口只读已经生成的产物，不启动 CLI、mock、Docker 或 cargo，不修改原始证据。
`c1-cron/compare.mjs`、`c1-online/compare.mjs`、本目录 `compare-https.mjs` 的分母
分别为 14、8、8。输出目录必须不存在；每次写出 `comparison.json`、每格左右投影，
原始输入文件 SHA-256 和完整差异均保留。`SAME` 仅说明指定投影相同；必须同时看
`productPassed` 和 `limitations`。两轮 TS 都挂起时可以 `SAME`，但
`productPassed=false`，绝不是产品成功。

在完整固定采集结束后执行，路径替换为 E0 原始证据路径：

```sh
node src/pilotdeck-cli/tests/fixtures/c1-cron/compare.mjs \
  --self CRON_TS_A CRON_TS_B --out NEW_CRON_SELF_COMPARISON
node src/pilotdeck-cli/tests/fixtures/c1-online/compare.mjs \
  --self ONLINE_TS_A ONLINE_TS_B --out NEW_ONLINE_SELF_COMPARISON
node src/pilotdeck-cli/tests/fixtures/c1-entry/compare-https.mjs \
  --self HTTPS_TS_A HTTPS_TS_B --out NEW_HTTPS_SELF_COMPARISON

node src/pilotdeck-cli/tests/fixtures/c1-cron/compare.mjs \
  --parity CRON_PAIR CRON_PAIR --rust-sha FULL_CANDIDATE_SHA \
  --rust-binary-sha256 BUILD_RECEIPT_EXECUTABLE_SHA256 --out NEW_CRON_PARITY_COMPARISON
node src/pilotdeck-cli/tests/fixtures/c1-online/compare.mjs \
  --parity ONLINE_PAIR ONLINE_PAIR --rust-sha FULL_CANDIDATE_SHA \
  --rust-binary-sha256 BUILD_RECEIPT_EXECUTABLE_SHA256 --out NEW_ONLINE_PARITY_COMPARISON
node src/pilotdeck-cli/tests/fixtures/c1-entry/compare-https.mjs \
  --parity HTTPS_TS_A HTTPS_RUST --rust-sha FULL_CANDIDATE_SHA \
  --rust-binary-sha256 BUILD_RECEIPT_EXECUTABLE_SHA256 --out NEW_HTTPS_PARITY_COMPARISON
```

固定 TS 仍为 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，Node 为 22.22.0。
`--self` 拒绝同一目录跟自己比较。cron/online 的 `--parity` 可指向一次 `--side both`
生成的同一根，两侧分别选择 TS/Rust；也可指向分开采集的根。Rust 源码 SHA 和二进制
SHA-256 必须从同次构建身份记录传入，比较器不会把“现在 HEAD 是这个 SHA”推断成
“这个二进制由它构建”。退出 0 为 `SAME` 或精确已接受差异，退出 1 为 `DIFF` / `INVALID`，
退出 2 为参数/输出目录错误；不完整输入不能缩小分母。

比较前会重新核对原始 stdout/stderr 哈希、完整场景名单、driver/cases 哈希、正式
命令参数、逐格断言、WS 原始帧与冗余状态、进程开始/结束/信号/观测边界和输出总字节。
只映射记录中的 fixture 根路径及 WS envelope 中真正出现的 request UUID；request、
response、event 必须使用同一映射。用户消息里长得像 UUID 的文本仍比较。观察日志的
时间和 PID 不作为业务内容比较，但信号必须属于登记的 CLI/子孙，配置的期限、自然
退出、cleanup、close 和残留仍比较。stdout 分块只核对总字节，完整字节文本参与比较。

online 会比较所有模型调用的 HTTP 方法/URL/头/原始 body，包括辅助标题调用和完整
system/messages/tools；不会按主 turn 数删调用。`content-length` 先与原始 body 核对，
再按根路径映射后的 body 重算。初始/最终快照先核对每个文件 hash/长度/base64，再比较
完整文本或二进制及符号链接；未知文件、JSON 排版、字段顺序、时间和内容 UUID 不被略过。
首次实测如果发现合法生成时间/标识，必须带原始差异和对应来源另行审查窄映射。

HTTPS 会核对真实目录中的 YAML/凭据与 result 副本，比较 `home`、`config`、`project`
整个树和每个请求的全部头/body。只归一 SQLite ExperimentalWarning 中的 PID，不移除
警告本身；TS/Rust 警告差异和 transport 默认请求头差异会如实报出。已登记的
`D1-SETUP-WEIXIN-HOME-001` 只用于 `weixin-missing-pilot-home`：TS 必须是精确 ENOENT、
初始 YAML、无凭据及已知默认目录；Rust 必须与同轮正常微信成功格的输出/文件一致，
仅父目录换到 custom-pilot。错误消息、额外文件、账号或配置不符均不接受；前置提示、
请求、stderr、退出和回答数仍比较。账本 hash 写入比较报告。

现有 HTTPS 采集器未记录源码 SHA、Node 可执行哈希、driver hash，也未独立保存原始
stdout/stderr 字节和请求 JSON 原文。比较器会显式报告这项证据限制，不能补造缺失数据；
因此 HTTPS 即使语义 `SAME`，也只给 `semanticPassed=true`，恒保留
`provenanceComplete=false`、`productPassed=false`；exit 0 仅是语义比较完成。
E0 必须保留采集命令、固定输入和构建身份记录并另行验证。比较器也不会凭预期给 cron/online 的
挂起登记差异，目前不含这种豁免。正式 E0 首次运行、两轮自比与 Rust 对照仍须执行。

装置自测只构造带明显 synthetic 身份的临时证据，不启动产品或网络：

```sh
node --test src/pilotdeck-cli/tests/fixtures/c1-entry/compare-common.test.mjs \
  src/pilotdeck-cli/tests/fixtures/c1-entry/compare-https.test.mjs \
  src/pilotdeck-cli/tests/fixtures/c1-cron/compare.test.mjs
```

覆盖精确分母、漏/重格、错误输出/hash、错 RPC、错模型请求/落盘、取消失败、未知字段、
真实 compare 子进程的故障注入非零退出、挂起不得变成产品成功，以及已接受差异边界。
这些只是比较器测试成绩，不计作 14/8/8 产品通过。

独立复核记录：首次 17 项合成自测通过后，review 发现真实 cron driver 的 TMPDIR 是
侧目录本身，而旧合成夹具多了 `/tmp`，掩盖了比较器只接受后代路径的错误。现已精确
支持侧根或其后代、修正合成形状并拒绝 `ts-evil` 等相似前缀。review 还发现输出父目录
的软链接可以指回输入树；现先 realpath 已存在父目录再检查，公开 CLI 负控验证输入
目录没有新增报告。先前合成通过不构成这两个边界正确的证据，均属装置错误而非产品
失败；正式采集仍待 E0。
