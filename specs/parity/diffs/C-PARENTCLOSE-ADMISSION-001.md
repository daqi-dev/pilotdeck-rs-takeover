---
id: C-PARENTCLOSE-ADMISSION-001
---

## 类别

不可避免的基线双终态定标补充；仅用户批准的 Original53 父关闭场景准入及配套输入闭包修复。不是 Rust 产品行为豁免，不排除场景、不降低分母或遍数。

## 基线依据

固定 OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85。`src/agent/sub/SubAgentSession.ts:213` 将 abortSignal 交给子 turn，`:239` 识别 aborted 结果；这两处源码不是双类实测的替代。真实参考 `_tmp/20261113-parentclose-20/archive/r01..r20` 的原 `run.json`、summary 及 `sidecar_parent_close_after_admission.pilotdeck-current-sidecar.jsonl` 观察到 completed 2 条、cancelled 18 条；预声明20份 TS 全部190对原G03为154类内PASS、36跨类FAIL。固定锁及全部 trace SHA 见 `_tmp/20261113-parentclose-candidate/ts-reference-lock.json`。原 TS5 十对自比及 S52 保留于 `_tmp/o53/ts-self.json`。

## Rust 行为

只改评分装置：`scripts/parity/score.py` 的 `parentclose_admission` 和该场景分支使用独立 TS 参考池／登记锁；`parentclose_policy.py`、`parentclose_oracle.py` 要求有效身份／健康／日期和真实 admission→child request→parent close→无副作用→operation/terminal 链。Rust 必须匹配其业务终态所对应的唯一确认类的全部成员。第二次实际使用的校准必须等于首次锁定结果，输入 hash 重新核验并纳入评分最外闭包。未改变 Rust 产品、G03 或 cancel 原策略。

## 范围

仅 `sidecar_parent_close_after_admission`。至少20份同日同源独立有效TS，两类均至少2成员，完整类内／类间矩阵；parent 与 cancel 两池及锁独立，不与正式TS5复用。只适用于原自比 S 恰为52且唯一 excluded 为 parent-close；未来 S53仍拒绝，不能自动扩大。补充绑定原 self.source 下同一 r1..r5及每轮全部53条输入 SHA。继续53场景、完整5TS＋5Rust；新终态、部分成员匹配、日期／身份错误、跨池复用或缺ledger拒绝。`controls/run_all.py` 配套仅随原trace复制原run.json；原缺失／错误ledger拒绝不变。

## 机器判据

`score.py` 正常入口要求两池两锁；`check_ruler.py --parentclose-supplement` 独立重读原件，保留S52。私有六文件身份清单 `_tmp/20261113-parentclose-candidate/candidate-manifest.json` SHA256 `f30fabbfcf7b15479641af7dbcee749aabd9a424ba7a6ed67617191799c19409`。16 integration控制、5 ruler破坏控制实际通过，包含错TS轮和两次校准间输入变更；旧62 controls实际通过。普通公共入口复核保存在 `_tmp/20261113-parentclose-public-application/`，未完成时不得冒称公共控制已过。单Rust对18条cancel类通过只是诊断；不代表真实五Rust全评分。冻结检查／独审／正式5＋5仍是独立门，所有原FAIL保留。

## 批准记录

用户于2026-11-13明确回答：「批准精确父关闭准入及冻结（推荐）」。持久批准指针 `state/decisions-20261113.md:95`，提案 `_tmp/20261113-parentclose-candidate/ROOT-APPROVAL.md`；批准限本条和精确六文件及必要冻结重登记。原S52及未来S53拒绝边界明确保留。没有推送、发布或直接总体PASS授权。
