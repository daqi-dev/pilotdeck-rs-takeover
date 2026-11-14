---
id: B3-WEB-FORK-QUEUE-RECOVERY-001
---

## 类别

固定TS现有网页恢复主链的两项bug修复：旧会话深链误落空白聊天主区；已修复历史的失败排队消息在显式Continue时仍重放同一旧执行失败。无新外部协议，不放宽Rust幂等和生命周期约束。

## 基线依据

固定cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85的ui/server/projects.js:236,313只提供最新5会话预览；ui/src/hooks/useProjectsState.ts:629–646只在已加载sessions寻找URL目标。实际F06旧链接落空General composer，保留_tmp/20261114-fork-recovery/F06-route-first-failure截图。ui/server/pilotdeck-bridge.js:2553起resumeInputQueue复用item.runId，Rust submit.rs:512–524正常按相同runId重放旧失败；F09已完成实际离线修源，Continue仍返回旧失败且未读新历史。retry-triage/diagnosis与固定TS真实QueueManager/sidecar run-03复现证据保留，不称另跑了TS全栈。

## Rust 行为

Rust61完整保留accept_turn幂等行为。修补位于scripts/backend-web-integration的保留TS前端及runtime精确overlay。路由目标不在preview时使用既有分页定位，定位完成前不挂可发送主区。显式Continue只有权威failed、完整registry/native历史证明该attempt未接纳、稳定状态和末次inactive均成立，才给首项分配新执行runId，保留消息item.id、正文、顺序及acceptedInputIds，strict sidecar持久化后才派发。权威runGet明确不存在的普通queued项保留原runId恢复。

## 范围

新增分页fallback的缺失、错误、跨项目多重命中、重复/空页与陈旧导航结果均保守拒绝；原preview命中规则不改变。未知run状态、已接纳/已执行、active、历史不全、版本变化、持久化失败不换attempt，仍暂停；不会自动Continue、删除旧registry或擦除历史。保留现有UI用户点击入口与sidecar格式。根因修复不自动豁免其他TS/Rust差异。

## 机器判据

正式check-route-recovery.mjs的4个实际React hook控制覆盖旧分页定位、缺失、陈旧响应和503；runtime/verify-queue-retry.mjs的21项使用实际TS QueueManager与真实sidecar，gateway证据接口mock，证明persist-before-dispatch、ID/正文/顺序保留、unknown/active/accepted/race/persist失败及并发Continue保护。正式loader source/result SHA必须匹配。F06/F07/F09实际CUA、真实RunRegistry/native和provider唯一派发另行验收；控制通过不能代替页面PASS。

## 批准记录

2026-11-14 root依据用户项目内决策自理、保留历史和排队消息并修复主链的明确授权，自主接受此精确bug修复，见state/decisions-20261114.md追加“Web旧会话定位与显式队列恢复”。Sol/Terra实现、Astra独审route-recovery-review.md与queue-attempt-review.md均源码PASS。不是用户逐字批准具体实现；本条不标记尚未完成的页面或真实LLM验证通过，不改旧冻结文件和判分器。
