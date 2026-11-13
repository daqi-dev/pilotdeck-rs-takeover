---
id: O1-AO-EXECFAIL-PREFIX-001
---

## 类别

用户批准的既有 AO 完整 UUID 截断关联规则有限场景扩展；不改变普通 UUID regex、业务 prompt 或排序。

## 基线依据

cc7c4e6 的 AO discovery 完整 prompt 含动态 plan/run UUID，listing metadata 按1200 UTF16单位截断。always-on-fire-execfail 原 TS2/Rust3实际可验证完整2038单位 prompt、provider/accepted/state/projection关系，六个 firstPrompt/lastPrompt 残片差异来自同一被截 UUID。原私有证据和7项错误scene/turn/listing/provider/anchor/state/歧义前缀负控见 `_tmp/20261113-global-delivery-review/ao-execfail-prefix-candidate/`。

## Rust 行为

沿既有 AO discovery 业务及 listing 截断，正文不修改；仅已证完整同 run UUID 的可见残片继承同别名。

## 范围

`ao-discovery-prefix-policy.mjs` 的有限场景集合仅由 always-on-fire 增加 always-on-fire-execfail。完整 accepted input/model 请求必须相同真实 full prompt，各 metadata字段精确等各自 prompt.slice(0,1200)，run/provider/state/anchor/两 metadata和projection完整关联，前缀无歧义后才建立 WeakMap descriptor。只替换残片同别名并保UTF16截断长度，不将整字段volatile。

## 机器判据

现有 `ao-discovery-prefix-policy.test.mjs` 与实际候选控制保持；正式比较器仍从同一raw内部生成 descriptors/full prompt校验，再执行原 normalizer。错误完整正文、listing长度、run/scene/anchor、歧义残片均拒；其他所有实际字段仍比较。私有原完整比较器执行记录不冒正式重判。

## 批准记录

2026-11-13用户明确批准 A 组，`state/decisions-20261113.md:59` 载明本项有限场景扩展。不得推至其他 AO/cron 场景，也不处理执行失败诊断正文。原AO DIFF6和全部raw保留，不改既有账本条目或冻结 aggregate/FROZEN。
