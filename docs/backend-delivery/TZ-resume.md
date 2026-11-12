# TZ 恢复：缺失 legacy 链接的同源解析

任务负责人：native_regression；起点为根 core 候选 `977026b2`，完整集成身份由根记录；交付提交待根集成。范围仅 `pilotdeck-transport/src/system_clock.rs`、`tests/system_clock.rs` 与本记录。无 Cargo 依赖/协议变化，不修改冻结判据、oracle 或系统目录。

## 首败与依据

E0 报告 builtin_tools 在越过 PDF 后的 `time-alias` 首败：Rust 对 `US/Pacific` 返回 invalid timezone，固定 TS oracle要求成功。`builtin-tools/generate.mts:423` 使用 US/Pacific；oracle time-alias 在 `1767225600000` 要求 `2025-12-31T16:00:00-08:00`。固定基线 `src/tool/builtin/getCurrentTime.ts:36,57,100` 以 Intl 处理时区。旧 `A8.3-CRON-TZ-ALIAS-001` 末尾明确未批准，故不能据此放宽判分。

E0 本次只读核对 cb8：`/usr/share/zoneinfo/tzdata.zi` 版本 2025b，第4577行精确为 `L America/Los_Angeles US/Pacific`；alias TZif缺失、canonical存在。

- tzdata.zi SHA256：`a7b113737e97a9b58f9f040a1b1e7e3af6e800649b1c381b601a9c69d425dc70`
- America/Los_Angeles SHA256：`68977bb9ad6d186fefc6c7abd36010a66e30008dcb2d376087a41c49861e7268`

官方主来源核验：[IANA 2026a backward](https://data.iana.org/time-zones/tzdb-2026a/backward) 第125行定义该 target/alias；该文件说明 backward 可选，用于兼容旧名称。[IANA zishrink.awk](https://data.iana.org/time-zones/tzdb-2026a/zishrink.awk) 第165-168行将 Link 缩写为 L。2026a仅用于核验记录格式与这一链接含义；实现实际读取宿主同源2025b，不引入另一版本的时区规则，不复制参考树或外部映射表。

## 实现

原 resolver 仅遍历已安装的 zoneinfo文件；split tzdata省略 legacy symlink即拒绝有效别名。本次保留直接文件优先，只在文件路径缺失时读取同 root 的 tzdata.zi，接受 L/Link target alias记录并按既有case-insensitive行为解析到canonical TZif。链最多32步并检测循环，路径组件拒绝空段、绝对路径、`.`、`..`；metadata和目标均须真实路径留在同一root，读取沿用1MiB完整性上限。未知别名、缺数据、坏UTF8、循环、越界路径继续失败，无UTC或固定offset猜测。

新增 `timezone_tzif_from_root` seam与既有 `bounded_zone_read`同类，仅用于替换根目录测试，不读取HOME或修改环境。生产仍使用固定 `/usr/share/zoneinfo`。本批只修named timezone路径，未扩展process TZ路径或声称整个系统tzdata与固定ICU各历史时间均等价。

## 测试与状态

新增三项真实临时root测试：

1. 无alias文件时，L、Link链、大小写折叠取得canonical完整字节，并在三个wall时间取得相同有效offset；直接文件优先。
2. 循环、缺target、缺metadata、绝对/parent/空组件路径失败。
3. 目标或metadata的symlink逃逸同一root失败。

fixture采用既有手工TZif规则，不假设宿主具备任何alias文件。`rustfmt --edition 2024` 两文件通过；`git diff --check`通过。未运行Cargo，测试尚未编译/执行，不能标完成。

| 层次 | 状态 | 未闭合 |
| --- | --- | --- |
| 接口 | 待集成 | 原resolver接口不变，增加root seam |
| 实现 | 待验证 | 代码已落盘，待编译 |
| 接线 | 待验证 | named resolver实际路径已接同源metadata |
| 验证 | 待验证 | 三项seam、现有system_clock及builtin_tools需E0固定SHA执行 |
| 交付 | 待集成 | 若发行环境连tzdata.zi也无，仍需完整tzdata来源/布局，不假称支持 |

下一步：根审阅集成后，E0在固定新SHA运行transport system_clock测试及原builtin_tools回归，保留977首败；若无同源Link数据的发行布局另行补数据源，不加入任意映射、不修改oracle。
