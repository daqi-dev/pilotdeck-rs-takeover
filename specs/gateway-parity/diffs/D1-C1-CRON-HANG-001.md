---
id: D1-C1-CRON-HANG-001
---

## 类别

已接受的基线挂起 bug 修复：正式 cron 已收到并输出完整业务结果/使用错误后仍持有远程 WS。Rust 在输出相同结果后关闭自己的 WS 并自然退出。本条仅准入下述固定原件的精确修复行为，不改变严格14格DIFF。

## 基线依据

固定 cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85、Node22.22.0，`pilotdeck.ts` SHA88c672b94f32c53d7bf5f188c62ef82965291aa55156e9032a43ae44198c8182：297行 main await handleCronCommand 后return；363行在判断四子命令/使用错误前 connectRemoteGatewayIfAvailable；363–424各分支return，没有finally/close。626行顶层catch只输出message并置exitCode1。probeServer.ts:33–45创建remote gateway；GatewayWsClient.ts:168有close，但该cron路径没调用。此机制与正式观测一致，不以源码猜无限挂起。

原件 `_tmp/20261113-e0-resume/c1-cron-collector49-formal-first/cron/`：TS-a/TS-b各14，额外pair TS第三轮14，共42次真实TS CLI。每次先取得完整准确终端输出，在之后实际1500.257–1501.970ms仍观察child alive、ownedWS open=1、没有close；随后驱动对本次spawn PGID发送一次SIGTERM并wait close。该signal是外层清理，TS productPassed=false、naturalExitCode=null保留。没有证明无限永不退出。

collector49只修mock.stop等待owned socket close才seal，原28次result/journal不一致的INVALID首件独立保留，不把它们倒推为有效。新TS self14严格SAME但productPassed=false；新pair14严格DIFF仍保留。

## Rust 行为

实际Rust46 c88650f6d89bd839d900e6256aa65bdca82e0b3d，Linux arm64 release CLI SHA8ec809aee94c5f0ace1b490eb7bc49c1239e509849d7e90c7f549dce14cb3b93，独立release-record/source/bin证据已核。14次真实CLI全部自然退出，无外部signal/timeout：六成功及参数优先格exit0，四使用错误与四RPC错误格exit1。全部有实际empty close payload、socket closed，然后mock cleanup；stdout/stderr、RPCmethod/UUID引用/params/所需response、隔离环境相同。不是一律把exit1或timeout接受成0。

## 范围

仅固定本批三份capture/14cases（共42TS+14Rust）及其精确readset/source/judge/buildreceipt/owner/实际tree。公共精确入口的可信深冻结源码登记锁384个原judge读键、额外63个原件（含56个token文件、三个exactCID的result/docker全链及postflight），审98个home/project实际树。只接受已验证TS observed-hang与Rust自然预期退出的生命周期差，以及这条close/cleanup事件后缀顺序差。所有HTTPprobe、hello、RPC和response原字节仍按固定cases逐条核，使用错误格也必须已hello且零RPC。原project空树与home唯一已知server-token精确哈希不变。

不得接受TS未到终端、启动超时、观察窗不足、没有liveWS、提前/错误PGID清理；不得接受Rust过早退出、缺RPC/response、错误exit、signal/timeout、缺WSclose或额外输出。PID/PGID来自即时spawn/journal和隔离exactCID，不声称有OSbirth字段或通用PID复用安全证明。该条不是任何其他timeout/CLI挂起的通用豁免。

## 机器判据

公共独立入口为 `src/pilotdeck-cli/tests/fixtures/c1-cron/accepted-cron-hang-exact.mjs`；同目录 `cron-hang-registration.mjs` 深冻结全部可信常量，包含原件读集/额外文件/tree/旧judge闭包/构建记录，调用者不能提供 manifest/Map 自授权。原judge不改，先完整重验和身份绑定，再逐原件核terminal→1500ms观察→ownedTERM→processclose与自然Rust预期退出，最后才处理精确已核生命周期/事件尾部差。原strict self14SAME/productPassed=false与pair14DIFF分开输出。

```sh
node src/pilotdeck-cli/tests/fixtures/c1-cron/accepted-cron-hang-exact.mjs \
  --evidence-root /absolute/copy-of-20261113-e0-resume --out /absolute/NEW_OUTPUT
node --test src/pilotdeck-cli/tests/fixtures/c1-cron/accepted-cron-hang-exact.test.mjs
```

公共入口实际正控已复核固定三份capture，共42TS+14Rust，读取前后SHA和完整384读键集合一致、98home/project实际树一致。52tests包含14真实case正控、早Rustexit/缺RPC/缺response/错ID/参数/使用错误exit0/缺WSclose/TS无terminal或startup timeout/观察不足/错PGID/提前TERM/KILL升级、不可变授权、缺/增读键和实际public CLI的raw/source/build/owner/tree/伪manifest负控及恢复正控。所有控制仅离线数据/既有mock，不新采产品、网络、API或Cargo。输入/代码前后哈希不是恶意并发写者CAS证明。

## 批准记录

2026-11-14，root依据当前用户项目自主决策和基线挂起bug修复授权，正式批准本批精确固定原件范围；这是项目内决定，不写成用户逐字审批。持久记录 `state/decisions-20261114.md:129`「C1 Cron 固定原件的终端输出后连接未关闭」。私有语义非作者审查 `_tmp/20261114-cron-hang-independent/review.md` SHA256 `03aac51e5d66486bfc35bcc578bffc1b09619caa583818f3532bcfd8012c50f4` 无固定范围阻断；公共迁移差额由同一非作者独审后root统一冻结。原strict14DIFF、42次TS observed-hang与外层清理收据保持原样，不准入别的CLI/场景timeout。
