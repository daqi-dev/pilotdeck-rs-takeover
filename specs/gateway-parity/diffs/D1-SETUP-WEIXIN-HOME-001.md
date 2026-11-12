---
id: D1-SETUP-WEIXIN-HOME-001
---

## 类别

明确凭据保存失败缺陷。基线支持PILOT_HOME却只创建默认home目录，在自定义PILOT_HOME首次登录时把已成功的登录结果丢弃，并输出误导的网络失败。

## 基线依据

固定 cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85 的 src/cli/commands/gatewaySetup.ts 微信保存路径：使用PILOT_HOME决定weixin-credentials.json目标，但mkdir针对os.homedir()/.pilotdeck。

本轮正式 dist/src/cli/pilotdeck.js gateway setup weixin 入口，Node22.22.0、独立HOME/PILOT_HOME，仅data: preload替换微信HTTP fetch为固定QR/confirmed本地mock，不访问真实账户。PILOT_HOME尚不存在时登录确认后输出“微信登录失败: ENOENT ... open '<isolated>/pilot/weixin-credentials.json'”并退出0，凭据未保存。原始stdout/stderr、路径和退出在 _tmp/20261113-c1-setup/weixin-missing-pilot-home.json，驱动probe-weixin.mjs。

## Rust 行为

src/pilotdeck-cli/src/gateway_setup/persistence.rs 为实际credentials目标创建父目录后保存。自定义PILOT_HOME首次登录可完成凭据和渠道配置保存，输出既有成功提示。其他文件字段、已有配置保留和失败提示不扩大调整。

## 范围

仅微信setup首次登录的实际凭据父目录不存在。不得把无效凭据、失败/取消登录当成功；不得把目录或权限错误吞掉；默认HOME路径的既有输出和落盘不变。

## 机器判据

gateway_setup/flow_tests.rs 的微信成功/失败测试使用mock确认结果及独立PILOT_HOME，要求实际凭据文件和adapter配置写入，取消或网络错误不写。三渠道正式HTTPS mock验证由E0 Docker私网执行并记录；本条登记不冒充该待验层已通过。已运行setup模块和5个既有凭据正式入口场景，结果见docs/backend-delivery/C1.md。

## 批准记录

2026-11-13 用户 docs/plan/backend-completion.md 明确允许修复崩溃、挂起、丢数据及与基线类型/文档/测试矛盾的bug；本条属于已成功登录数据无法保存且PILOT_HOME配置不生效的明确缺陷。按该授权实施，普通配置行为保持。
