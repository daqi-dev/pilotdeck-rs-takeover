---
id: A7-NATIVE-ARCHIVE-U8-BASE64-001
---

## 类别

基线 bug 修复。固定 TS 的宿主 artifact read 合同显式允许 `Uint8Array`，公开 native archive 返回值显式声明 `encoding: "base64"`，但该合法输入的实际公开输出是十进制逗号字符串。当前 Rust 把宿主原始字节编码为真实 base64。差异仅纠正该内容编码矛盾，不是一般输出归一化，也不授予其他判据豁免。

## 基线依据

固定基线 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`：

- `src/gateway/storage/AsyncTranscriptStorageAdapter.ts:88–90` 的可选 `toolResultArtifacts.read` 返回 `Promise<Uint8Array | undefined>`；`Buffer` 是允许的字节子类型。该文件 SHA256 `1071059df586535fccd459afe1cb0b0e14c209f46038da0d964fe260c6cdb423`。
- 同文件 `:316–329` 将宿主 read 值以 `.slice()` 转给原 session storage，保留这两个具体子类型。
- `src/gateway/client/InProcessGateway.ts:2575–2586` 的公开 `nativeArchiveArtifact` 先验证 session/name/maxBytes，经过原 authorizer，再调用原 callback。该文件 SHA256 `bbeabd02733d2cba2020f64fdcff31bcb0defd589aa695aa80a6041900859c7a`。
- `src/cli/createLocalGateway.ts:1223–1235` 经原 registry/storage 读取宿主字节，取 `stored.subarray(0,maxBytes)`，再调用内容的 `toString("base64")`，并固定声明 encoding。该文件 SHA256 `54c2f856936733ef9c04d9c995a32317ed14a0d4070d202a0c2365447491869f`。
- `src/gateway/protocol/types.ts:1160–1166` 的公开结果 `encoding` 固定为 base64；该文件本次只读核 SHA256 `e3c9d4e11b3a2acbc3dd5445896bbaef1cef332a82cfe11d4da0668de4de589a`。原测试 `tests/gateway/capabilities.spec.ts:783–785` 把公开 content 按 base64 解码后断言原正文。

实际 witness 位于 `_tmp/20261114-native-archive-byte-oracle/attempt-2/`。它使用同一组原始字节 `[0,1,2,127,128,255]`，宿主 read 分别返回 `Buffer.from(bytes)` 或 `new Uint8Array(bytes)`，经过完整的 `createLocalGateway → gateway.nativeArchiveArtifact → authorizer → 原 callback → 原 adapter artifactStorage.read`，不是单独的 JavaScript toString 演示。

| 宿主 read 值 | maxBytes | 实际 content | encoding | bytes | truncated |
| --- | ---: | --- | --- | ---: | --- |
| Buffer | 3 | `AAEC` | base64 | 6 | true |
| Buffer | 6 | `AAECf4D/` | base64 | 6 | false |
| Uint8Array | 3 | `0,1,2` | base64 | 6 | true |
| Uint8Array | 6 | `0,1,2,127,128,255` | base64 | 6 | false |

空 Uint8Array 实际返回 `content:"", bytes:0, truncated:false`；缺失宿主值返回原 `ARCHIVE_ARTIFACT_NOT_FOUND`。原 authorizer 与宿主 read 各执行 6 次；网络 guard 记录 0。Node22 自然退出 0，1.069745 秒，未超时，已 reap；5 个运行相关原文件 SHA 前后相同。

不可覆盖的原件：

- `_tmp/20261114-native-archive-byte-oracle/attempt-2/observation.json` SHA256 `815f32f6e825e897401720cddf67b16fefa613c27f946ad719d8d902602a3485`。
- `_tmp/20261114-native-archive-byte-oracle/attempt-2/receipt.json` SHA256 `501a7ef87fdaf3a325b7c2c2f6f9e5c25b4853d1c0681b9af80ac46f03bd0689`。
- `_tmp/20261114-native-archive-byte-oracle/attempt-2/oracle.mts` SHA256 `c78de7a3ca9e2352c27d7ed9d31d18ed392cb2d53e5189a3281d5c3a1442f748`。
- `_tmp/20261114-native-archive-byte-oracle/attempt-2/invocation.json` SHA256 `fc121eb2697dfac0ded23976486450b15b554e880978d45b28d696a8fc161bd8`；另含 stdout/stderr/owned-process 原件。

`_tmp/20261114-native-archive-byte-oracle/attempt-1/` 是本作者错误的前置配置：自定义 getProjectChatDir 却未同时提供匹配 createSessionStorage，原校验拒绝 chatDir 不一致，自然退出 1；它没有进入字节编码结论，不计成功。其原件保留，receipt SHA256 `932b2327016e443b1909391cf861e3e98b46d95c43aedcd2c73bbc8066ef5f0f`。第二次改用原默认布局，未改原 TS。

## Rust 行为

`src/pilotdeck-gateway/src/native_archive.rs:581–589` 现有 `STANDARD.encode(&bytes[..bytes.len().min(limit)])` 根据真实字节生成 content；编码标签、原字节总长、截断标记保留原合同。宿主能力接口使用 Rust `Vec<u8>`，不增加 JavaScript Buffer/Uint8Array 构造器身份枚举以复刻坏输出。

当前正在测试的 Rust 源码未因本 oracle 修改。编译通过是已观察事实；此新合成 U8 对照的 Rust 公共出口行为控制尚未实际执行，不能写作已经通过。

## 范围

仅宿主 `toolResultArtifacts.read` 返回普通 Uint8Array 对应的 `nativeArchiveArtifact.content` 编码修正。Buffer/默认文件系统的正常 base64 结果保持一致；空、缺失、maxBytes 限制、bytes/truncated、权限 authorizer、retention、session/project 归属和错误合同不扩大。保存的原始宿主 blob 没有因该 TS bug 被证明删除；已证明的是这个公开编码出口无法按标签恢复该输入的原字节。

不放宽 artifact managed path、fork 拷贝条件、其他 API 或通用 judge。固定 TS 的 U8 逗号输出保留为真实 golden；比较时只能以本 ID 对这个明确 bug 使用独立修复断言，不把整段输出忽略。

## 机器判据

TS witness 的实际 6 行输出和无网络/自然退出/固定 SHA 原件如上。`_tmp/20261114-native-archive-byte-oracle/attempt-2/oracle.mts` 中的明确断言固定了 Buffer 与 U8 的差异，并同时固定空、缺失和截断。

Rust 后续必要控制建议：经正式 `LocalGateway::start_with_session_storage → LocalGateway::request("native_archive_artifact",...)` 注入同 6 字节宿主 Vec，maxBytes 3/6 应得到 `AAEC`/`AAECf4D/`，解码后严格等于相应字节，保 bytes6 与 truncated true/false；空与缺失保持上表对照。该控制待协调者在当前正在执行的 checkpoint 后安排，不能冒称现有 portable archive/fork 测试已覆盖此 API。

不重跑原 9 大场景；如非作者需要复验，使用封存 invocation 的同 Node22、tsx loader 与隔离 HOME/PILOT/TMP，另开私有目录，保留原件，不覆盖此次观测。

## 批准记录

已由主协调依据本会话用户项目自主决策授权及基线 bug 默认修规则批准，持久记录为 `state/decisions-20261114.md:137–145`（决定标题“宿主原始字节的 native archive base64 编码”）。这不是另一次用户逐字审批。批准明确仅保持 Rust 真实字节 base64 的现行为，不复制 TS 的损坏编码，不新增类型身份枚举，不扩大其他字段/入口或比较器。

非作者已独立核验原 TS 公开入口、类型合同、6 行实际结果、运行隔离、源码身份和退出；收据 `_tmp/20261114-artifact-hydration-independent/native-archive-byte-oracle-review.json` SHA256 `3040e1adb72dd7857964210de67b408bb64885bd6a1e51b4e44aa825e1ccf779`。此次公共条目只从已审私有六字段草案机械迁移，原草案 `_tmp/20261114-native-archive-byte-oracle/diff-draft.md` SHA256 `c8bca689cf724ab481e3d0b9f6aeaedbe20128f7290579469ec37e57d780d1f7` 保留。

批准记录同时保留验收边界：本次登记时，相同六字节 max3/max6、空和缺失的正式 Rust 公共 request 控制尚未实际运行，须在新运行时集中验证；现有便携归档/fork 通过不能代替这个 API。未改变产品 src、原 judge、历史账本或 FROZEN，后续统一冻结由协调者完成。
