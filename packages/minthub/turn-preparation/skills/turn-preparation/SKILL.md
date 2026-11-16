---
name: turn-preparation
description: PilotDeck 纯回合输入准入与拥有权准备：权限上下文、计划附注、运行模式准备、初始化与模型准入校验。
---

# turn-preparation

## 能力

负责把原始调用输入准入为类型化 `OriginalTurnPreparation` 与 `PreparationEffects`，提供 `execute`、`initialization`、`model_admission`、`preparation` 与 `AdvertisedMethods` 查询。

## 先读什么

- `src/lib.rs`、`src/preparation.rs`、`src/execute.rs`、`src/initialization.rs`。

## 边界

- 纯准入与准备逻辑（`#![forbid(unsafe_code)]`），不创建执行运行时、不持有宿主端口或授权权威。
