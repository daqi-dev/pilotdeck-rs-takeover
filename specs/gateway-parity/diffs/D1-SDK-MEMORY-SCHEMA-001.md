---
id: D1-SDK-MEMORY-SCHEMA-001
---

## 类别

用户批准的固定 memory 标识/schema、采集行序与单 fileSize 派生兼容；不是任意 hash/JSON 字符串忽略，不改变产品存储。

## 基线依据

cc7c4e6 `context/memory/edgeclaw-memory-core/src/core/utils/id.ts:11-13` 以 timestamp:payload SHA1前10hex 构造 L0 ID；`src/service.ts:1041` 和 `core/retrieval/reasoning-loop.ts:58-59` 的 case/trace 标识含未采 Math.random。`scripts/bbx/run.mjs` 收集 SQLite 全行后按 JSON 文本排序，随机 ID 会改变两条 parent 记录的采集位置。原 memory TSself 已实际出现该差异。

## Rust 行为

固定原场景中三条实际 L0、三个 case/trace 和21 step 引用维持业务集合和引用关系。ID、时间及实际报告 token 数变化影响单个 parent JSONL 的真实字节长度，stat 应继续忠实报告该侧真实长度。

## 范围

仅 memory SHA `d654c045d164826de415eb7581325471b5b0b32b57d664a8069a1feb504e101a`。三 L0 必须逐侧核 session/query/payload、完整 schema/唯一性、SHA1公式和 collector 行序；只允许三 ID 及两条换位 messages_json。pipeline state 必须精确 schema、一一 case/trace/step 关联和全部非目标业务相同，仅该序列化 state_json 叶可准入；不声称逆算缺失的熵输入。12个实际 ISO 字段复用既有冻结规则，不新增时间范围。

单 fileSize 仅 `$.steps[6].frames[0].result.session.fileSize`；62 parent 行须60完整 typed producer加2独立严格 reviewer lifecycle，全部文本/UTF8 bytes/SHA重建及 stat 一致。它依赖已批准 context/window 完整证明，不推广任意 size。

## 机器判据

`observer-memory-witness.mjs` 内部调用真实 provenance/lifecycle，从 raw 独立产生七个精确路径；缺证、非唯一记录、错 schema/业务、错 case/trace/step、少行/新增字段、错误 byte/SHA/stat 均拒。`observer-memory-witness.test.mjs` 保留实际25控制及既有时间资格边界。原正/负控制、时间额外限制的首失败和修正事实位于 `_tmp/20261113-observer-memory-provenance/`，公共测试另行记录。

## 批准记录

2026-11-13 用户 A 组明确批准；见 `state/decisions-20261113.md:59` 及所引精确六范围候选。时间规则原许可不重写，case/trace bimap 是此次新增的严格 schema 许可。tree 键序、任意十位 hex、其他 SQLite 表和未知字段不在内。旧 raw/DIFF 不改，aggregate/FROZEN 不改。
