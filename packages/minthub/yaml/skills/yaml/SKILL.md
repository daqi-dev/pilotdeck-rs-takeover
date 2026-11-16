---
name: yaml
description: npm yaml@2.9.0 的忠实 UTF-16 Rust 移植：涵盖词法/解析/组合/Document#toJSON 与配置写回 stringify，保留深度保护与负零/NaN/孤立代理语义。
---

# yaml

## 能力

提供 `parse_document`、`parse_all_documents`、`Document::to_json`、`stringify::EditableDocument` 等核心 YAML 解析与编辑管线，全量覆盖 3,099 条基线差分样本与栈深度阈值测试。

## 先读什么

- `src/lib.rs`、`src/doc.rs`、`src/parse/`、`src/stringify/`、`LICENSE.yaml`。

## 边界

- 不调用 Node、不放开基线未使用的自定义 tag/schema 选项；在 V8 实测栈深度下界处显式触发 `RESOURCE_EXHAUSTION` 保护。
