---
id: B3-PROBE-ZH-TEXT-ONLY-001
---

## 类别

bug修复：模型探测旨在识别明确不支持图像的响应，固定实现只识别英文，厂商中文明确仅允许text时却归类unknown。

## 基线依据

固定cc7c4e6的ui/server/services/modelConnectionProbe.js looksLikeImageUnsupported只匹配英文unsupported/allowed values；实际国内glm-5.3文本POST200，图像POST400返回messages.content.type 参数非法，取值范围 ['text']，网页显示Text Passed、Image Undetermined。原件_tmp/20261114-provider-settings/zhipu-connection-task-first.json与ui-provider-audit06.jsonl及截图保留。

## Rust 行为

Rust58不改。保留TS UIserver的runtime readonly overlay新增modelConnectionProbe.js精确目标，仅该函数增加有限中文诊断+真实HTTP400条件，既有image判定调用传入response.status。旧英文行为保持，不修改请求或重试；loader仍对源/结果SHA fail closed。

## 范围

必须HTTP400、完整messages.content.type字段、参数非法、取值范围仅['text']，允许有限空白/标点/单双引号差异。不同字段、含image或其它值、附加未知/限流信息及其它HTTP状态不由新规则判unsupported；text失败不会触发新image成功。原4096输出预算、text后image顺序、模型catalog、协议与网络鉴权均不改。旧已生成task不会自动重算。

## 机器判据

scripts/backend-web-integration/runtime/model-probe-controls.test.mjs通过真实loader导入probe、内存mock返回正负响应：精确录样、有限格式变体、字段/取值/状态/附加错误负例、text失败、英文保持、text200后image400分类。正式runtime verify原11+新7组控制通过，见_tmp/20261114-provider-settings/diagnostic07/controls-final/receipt.json。source SHA feb25d6cd28a73c2b45c8cfad524241bece91bd10dbda1b7dda0c680967fc32f，result SHA a9d1797c4cf7f5b7af025d5fd30425feccc2db4d1612a20cfdc5c385970ea79d。mock不冒实际新API通过，未知500保留unknown。

## 批准记录

2026-11-14 root按本会话项目内决策授权及提供商真实接入任务接受此精确能力诊断修复，见state/decisions-20261114.md。保留原真实400/unknown首件，不改变冻结协议判分、不概括豁免其它错误。
