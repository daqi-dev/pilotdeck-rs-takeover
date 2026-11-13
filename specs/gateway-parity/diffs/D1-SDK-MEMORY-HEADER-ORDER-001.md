---
id: D1-SDK-MEMORY-HEADER-ORDER-001
---

## 类别

已批准的精确采集对象键序评分差异。不是已校准 volatile，不是所有 HTTP header 顺序的通用豁免，不改变产品请求。

## 基线依据

固定 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的 memory TS 原件 `_tmp/20261113-e0-resume/7bef-ffbb-batch2/agent03-sdk-memory-lifecycle/pair/agent03-sdk-memory-lifecycle.ts.json` 与 cb7a Rust 原件中，`provider.memory-gate[0..2]` 及 `memory-selector[0..2]` 的六个 headers 对象具有相同的全部键和值，method、url、完整 serialized body 及真实 content-length 一致，但键的插入顺序不同。固定采集器 `scripts/bbx/run.mjs:203` 记录 `req.headers`，没有记录 rawHeaders。没有同精确 TS self 中键序变化的重复实证，不把此条称为随机波动。

## Rust 行为

`scripts/bbx/observer-memory-policy.mjs::memoryHeaderObjectOrder` 在与派生值条目相同的固定 case/scene/TS 身份下，先核两组各恰好三个请求；逐侧重算 content-length，要求 method/url 和完整 serialized body 相同。每个 headers 必须具有固定十键，所有键值与 string/array 类型、array 次序深度严格相等，仅返回六个指定对象的 `<key order>` 差异路径。值变、少键、多键、重复表示变化、正文变化或其他身份均抛错。

## 范围

只允许 `$.provider.memory-gate[0..2].headers.<key order>` 和 `$.provider.memory-selector[0..2].headers.<key order>` 六处，不排序或改写原对象，不忽略任何键值或 body，不接受其他 header 路径。保留采集器可见的重复值表示；不声称原始 HTTP 重复行、大小写或 raw header wire 等价。未知差异继续正常比较，observer 顺序/UUID 联动不属本条。

## 机器判据

`node --test scripts/bbx/observer-memory-policy.test.mjs` 的合成控制明确只产生六个 header key-order 路径，并拒绝改值、缺键、额外键、string→array、正文改变后重算真长度、伪 length、额外请求与错误 case。真实原件一次验证收据 `_tmp/20261113-observer-memory-policy-application/controls.json` 对当前六对实际请求应用相同合同；不启动产品、不增加 TS 采样，不把控制通过写成整案 SAME。两个原件 SHA 在该收据中保留。

## 批准记录

2026-11-13，本会话用户明确批准三组精确边界；主协调者授权正式应用已独审 memory/header 候选。本条仅登记其中六个采集对象键序叶。用户已被明确告知缺少 same-exact-TS-self 变序实证与 rawHeaders 采集限制，批准对象是此精确评分差异，而非扩大既有 volatile 授权。最终私有候选 SHA256 `a2f8a53c7ea610e373486901a1f4a90ba33774249f87986b6a08d1e5ede5dee8`，SDK 独审无阻断；实施记录 `_tmp/20261113-observer-memory-policy-application/application.json`。不据此自动接受其他场景或全部 observer 差异。
