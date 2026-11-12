---
id: T1-SYNTAX-NATIVE-001
---

## 类别

有意改进：保留写后诊断能力，以原生解析器替代Node中的TypeScript/parse5/csv-parse。仅允许诊断说明的有限差异。

## 基线依据

固定cc7c4e6的 `src/tool/builtin/filesystem/syntaxDiagnostics.ts`，实际formatSyntaxDiagnostics输出在 `_tmp/20261113-T1/diagnostics-baseline.jsonl`。JS缺表达式、TS缺类型、HTML重复属性及CSV少列/引号未闭合均报告语法问题；合法TSX/HTML无问题。

## Rust 行为

`runtime/tools/builtin/syntax_diagnostics/native.rs`调用Oxc/已有HTML parser与原生CSV记录解析。保留 `Syntax issues detected:`、L行:列、最多三项及长度约束；说明文本采用原生诊断。HTML解析器只给行时列记1。

## 范围

仅write_file/edit_file成功文本中JS/TS、HTML、CSV/TSV诊断块。写入、事件、结构化补丁、权限、其它格式和CLI/协议均不在豁免内。无诊断与有诊断不可视为同一结果。

## 机器判据

`syntax_diagnostics::native::tests`覆盖八种JS/TS扩展、UTF-16定位、HTML合法/非法片段、CSV多行/转义/BOM/列数及混合换行。正式工具场景仍待追加并运行，不能因本条直接判绿。

## 批准记录

用户2026-11-13明确要求执行 `docs/plan/backend-completion.md`，其中“JS/TS、HTML、CSV/TSV 写后诊断允许有限的文案差异，但必须保留诊断能力”。本条记录该已有授权，不扩展为其它输出差异。
