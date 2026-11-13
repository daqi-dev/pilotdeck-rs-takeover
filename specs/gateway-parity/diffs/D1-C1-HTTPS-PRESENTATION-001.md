---
id: D1-C1-HTTPS-PRESENTATION-001
---

## 类别

已接受的原生传输/运行时表示差异。仅许可本条固定原件的八个出站请求头名称位置的字母大小写，以及固定 TS 入口 stderr 开头一次准确的 Node SQLite ExperimentalWarning 两行；不是 setup 请求、错误、凭据或持久化的业务豁免。Rust 不为模仿 Node 而输出虚构运行时警告。

## 基线依据

固定 TS `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`、Node22.22.0，由正式 `gateway setup` 子进程访问私有 Docker 网络内四个官方域名的 loopback TLS mock，无外部 API/真实账户。原件为 `_tmp/20261113-e0-resume/c1-final46-contract-r2/https/{ts-a,ts-b,rust}`：三份完整 capture 共24原始case。两轮 TS 自比严格8/8 SAME；TS/Rust 严格8/8 DIFF 保持原判。

`src/cli/commands/gatewaySetup.ts:226,440` 分别使用 `Content-Type`、`User-Agent`。实际原始请求另观测 `iLink-App-ClientVersion`。完整原件中其值、顺序、重复情况及原始 body 均保留；这里只把指定名称的三个实测拼法与 Rust 小写拼法关联，不从 HTTP 语义推导通用豁免。每轮 TS 的精确 SQLite 警告均位于 stderr offset0；保留原始 PID/字节，比较视图只按准确两行前缀一次处理。

## Rust 行为

实际 Rust 来源 `c88650f6d89bd839d900e6256aa65bdca82e0b3d`、Linux arm64 release CLI SHA256 `8ec809aee94c5f0ace1b490eb7bc49c1239e509849d7e90c7f549dce14cb3b93`。`src/pilotdeck-cli/src/gateway_setup/network.rs:128` 经原生 HeaderMap/HTTP 栈输出小写名称；Rust 不加载 Node SQLite，stderr 没有该运行时警告。八例正式执行的 method/URL/body/头值/stdout/exit/问答/配置和凭据文件均无其他差异。已登记 `D1-SETUP-WEIXIN-HOME-001` 仍按其原条件独立核实，不扩大。

## 范围

入口 `src/pilotdeck-cli/tests/fixtures/c1-entry/accepted-https-presentation-exact.mjs` 与同目录 `https-presentation-registration.mjs` 的可信源码常量硬锁三份 capture 的268个完整read-key/文件SHA、原judge/helper/capture源码和旧Weixin差异条目SHA、独立release-record及三份构建证据SHA。路径参数仅定位相同字节，不接受调用者提供 manifest/Map 或新的授权集合，不准入未来capture。

仅以下 `requests[index].rawHeaders[4]` 名称位置：三例 Feishu/Lark 的 request0 `Content-Type`；WeCom request0/1 `User-Agent`；三例成功/缺home/写配置失败 Weixin 的 request1 `iLink-App-ClientVersion`。Rust 原始名称必须恰为对应小写，其余名称/全部键、值、计数、重复、顺序、正文、响应、退出、落盘、command/env均比较。Weixin HTTP失败格没有头名称豁免，仅准确的警告前缀。

SQLite只对TS已PID映射的精确两行 offset0 前缀去一次。非前缀、变体、重复、CRLF、业务 stderr 和 Rust 同形警告都不得通过。原始文件不变，严格 self/parity 和 accepted 报告分别保存。输入/代码hash前后核不是对任意并发恶意写者的CAS证明；捕获的是HTTP应用头/body，不是TLS记录或chunk framing。

## 机器判据

在本批固定原件及构建记录均就位时：

```sh
node src/pilotdeck-cli/tests/fixtures/c1-entry/accepted-https-presentation-exact.mjs \
  --evidence-root /absolute/copy-of-20261113-e0-resume --out /absolute/NEW_OUTPUT
node --test src/pilotdeck-cli/tests/fixtures/c1-entry/accepted-https-presentation-exact.test.mjs
```

公共入口重新运行原严格比较器完整验原件，必须保 self8 SAME、pair8 DIFF，再要求准确16个比较行经本条表示处理后全同，268读键集合完整且没有额外读键。真实公共正控与纯数据内容/源码/原始字节/构建记录/调用者伪manifest负控须全部通过。不能用本条把任何新错误、缺失响应、错误exit、缺凭据或额外文件判绿，不对cron挂起作任何许可。

## 批准记录

2026-11-14，root依据当前用户的项目自主决策授权，明确批准本条固定批次的精确表示范围；不是用户逐字审批。私有语义非作者独审 `_tmp/20261114-c1-https-presentation-independent/review.json` SHA256 `bfe16be1b03c26c27c3487174e27b57413d7c9af6fa5a4c38b94d966e53563df`，原35控制重放及独立42内容负控通过。公共入口实现差额另经非作者审查及root统一冻结；旧8格DIFF、原件与旧比较器不改。
