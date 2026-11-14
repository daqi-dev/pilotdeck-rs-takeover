---
id: B3-PRESET-PROVIDER-URL-001
---

## 类别

有意改进，限定于本次用户明确要求修复网页 Coding Plan 接入的既有配置能力。公开 provider.url 已支持替代服务地址，现让预设提供商也能在网页编辑该字段；不把整个 preset 设计声称为固定基线的协议错误。

## 基线依据

固定cc7c4e6的ui/src/components/settings/view/modelPool/components/ProviderCard.tsx中connection-grid由!effectiveCatalogEntry包围，预设隐藏Base URL；catalogProviders.ts:203-207的zhipu默认全球普通API。ModelsSection.tsx:98-119保存已有provider对象，model配置解析优先显式URL。用户本次国内Coding Plan key保留原值、仅改为国内coding地址并恢复受控出口后，真实models及文本请求通过，说明配置必须能表达正确目标。精确源/结果SHA见scripts/backend-web-integration/baseline.json与patches.json。

## Rust 行为

Rust后端58不改。保留的TS React前端通过scripts/backend-web-integration/patches.json追加ProviderCard.tsx变换：预设和custom都显示既有Base URL编辑，protocol选择仍仅custom；effectiveUrl保留原默认及已保存值。参考resources不修改，不冒称UI server迁移。

## 范围

只开放既有URL字段；不改默认host、protocol、model catalog、鉴权或协议。已保存masked key不能跨URL/protocol复用的server scope校验保持，用户须显式重新输入凭据。没有新增任意文件读取或远程自动发现。

## 机器判据

scripts/backend-web-integration/provider-settings.test.ts经check-provider-settings.mjs运行4项控制：实际ModelsSection→ProviderCard保存非默认URL、normalize/preserve/mask后同scope重读再存key保持；默认回退/预设固定协议、非法URL拒绝、custom输入兼容。控件不执行HTTP scope-validator，跨URL保护仅源码复核，不能称已实测该负控。_tmp/20261114-provider-settings/frontend06/controls-02/results.json与independent-review.json，14.03秒正式构建exit0，原10补丁对象/raw前缀不变。

## 批准记录

2026-11-14 root按用户“项目所有决策自行处理”及本次“设置智谱Coding Plan失败、使用已授权key实际测试”的明确任务授权接受此窄改进，见state/decisions-20261114.md的提供商设置补验条目。没有改旧冻结判分文件，不为其他差异增豁免。
