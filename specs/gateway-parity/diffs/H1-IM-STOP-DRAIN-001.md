---
id: H1-IM-STOP-DRAIN-001
---

## 类别

挂起与资源泄漏修复：Signal、Mattermost、HomeAssistant停止或重载后，取消并排空该实例已经接纳的轮次和出站工作。

## 基线依据

固定cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85，真实CLI server、隔离HOME/PILOT_HOME和随机loopback模型/渠道mock。SignalChannel.ts:224-240、MattermostChannel.ts:202-220、HomeAssistantChannel.ts:325-341各自维护activeChats，但停止没有等待Gateway轮次的完整尾部。

实际证据：`_tmp/20261113-h1-signal/reload-first`与`reload-second`禁用后receive已关、健康服务仍活、provider连接仍开且run=running，重新启用也未清除旧连接；Mattermost `server-l/m` 的disable-held格在停止4.5秒后provider仍开，释放模型后旧adapter仍POST；HomeAssistant `stop-a/b`禁用后WS已关、health=200而provider仍开，释放后旧实例尝试通知并报not connected。这些观察发生在服务器存活期间，不是由驱动结束进程制造。

## Rust 行为

新三渠道通过`TurnGateway::submit`取得精确owned stream，停止同步关闭本实例入口，再取消该stream并等待其真实worker、持久化终态和出站工作完成。公共`cancel_and_drain`绑定原completion和活动guard，不按session名找可能已替换的后继。等待者被丢弃不能丢失清理所有权；失败保留精确能力供重试，不能用空任务表返回成功。只在真实排空后清本地互动状态。

## 范围

仅上述三渠道实例的停止、关闭和重载所拥有的在途工作。普通消息、错误文本、权限/询问内容、cron投递能力及重连间隔仍按基线；Mattermost仍不处理cron结果。本条不豁免正常收发、不放宽错误为成功，不允许取消其他渠道或同名后继会话，也不替代外层既有停止预算。Signal/HA出站所有权修复包含已poll后调用方暂停或丢弃的cron receipt。

## 机器判据

`tests/fixtures/h1-signal/capture.mjs --scenario reload-held`、`h1-mattermost`正式disable-held和`h1-homeassistant/capture.mjs` stopProbe的Rust侧要求：同一服务器仍健康，停止完成前旧provider已关闭，零迟到回复、零禁用后重连，重新启用可正常处理新输入。共同`h1_` owned测试以及三个渠道的Pending stop→drop→retry、失败后重试、worker异常因果测试不能被静态审查替代。Signal/HA另要求保留不再poll的出站receipt时，stop仍真实收口HTTP/WS资源。

截至登记，上述TS正式证据已取得，Rust候选和测试尚未集中编译/运行；不能因本条存在自动判通过。须随固定集成候选保存实际测试日志和负控结果。

## 批准记录

用户2026-11-13执行计划明确授权修复崩溃、挂起、丢数据及安全问题；主协调在本会话明确授权这三渠道owned取消和停止收口。这里限定为已有正式实测的停止后遗留资源，不以授权代替Rust验收。
