# Native SDK session directory plugins

固定TS依据：ProjectRuntimeRegistry.ts993-1023、1808-1847；ProjectSessionRuntimeBundle.ts247-279；PluginLoader.ts27-56。目录插件读取manifest、Markdown commands/skills/styles、hooks和MCP配置，不动态加载JS函数。此范围没有因旧JS函数排除而整体拒绝plugins。

local目录是无外发的data fixture，测试复制到独有临时home以便改变manifest/Markdown与构造canonical alias。MCP command为unused-local-mock，仅用于真实SDK配置状态机保留名/状态查询，不启动进程；hooks不实际执行。行为测试调用真实磁盘loader、生产SessionOverrides的原子config/plugins表、真实SdkMcpConfig及contribution lease/read_skill工具，未用固定假结果代替能力。

新sdk_plugins_tests.rs七项（Unix含canonical alias）：data贡献、精确路径/目录/加载/重复/名称冲突错误、alias冲突、session隔离MCP状态与保留名/clear、重配snapshot与project lease释放、失败重配保留旧状态/原子store、真实read_skill读取session view且不污染project catalog。测试尚未编译/执行，不作通过声明。

native公共接线由各唯一作者分别负责：sdk_config校验/prepare/skills/outputStyle与原子commit；assembly同一lease snapshot消费hooks/MCP/skills/commands列表并保留release；MCP状态/control同store注入。完整正式SDK提交、模型请求、MCP连接、取消/重启和slash command展开另须当前集成版本验证，组件测试不冒称全部入口完成。

InputProcessor模块证据：隔离HOME/PILOT_HOME，以固定`/opt/homebrew/Cellar/node@22/22.22.0_1/bin/node`实际加载只读基线dist InputProcessor，`capture-input.mjs`两轮7格（正文、无参数、空正文、未知slash、blocks、delimiter碰撞、BOM与孤立UTF16）和32k/8k截断hash精确自比；`verify-input.mjs`两正控exit0。--negative在实际processor调用之前替换extension command body，同一固定oracle校验exit1。原始产物位于`_tmp/20261113-sdk-plugin-input-oracle/`；proof记录Node版本、dist hash和当前driver hash。早期正控运行时driver尚无--negative分支，正控核心相同但未保存那版driver独立快照，不将当前hash冒充早期driver身份。

Rust plugin_input五项待编译：精确delimiter正文与重复展开、unknown/Blocks原值、注入与UTF16限长、session独立command集合、固定Node22 oracle及bounded UTF16 hash。host_factory新增真实peer测试验证input_transform一次处理后模型messages与accepted_input保存同样展开文本；该测试验证正式LiveSession传递时机，纯processor测试独立验证实际slash算法，二者仍不冒称完整SDK server/plugin配置验收。

最终使用同一当前capture/verify源码完成`final-a`、`final-b`（verify均exit0）与`final-negative`（verify exit1），`final-proof.json`固定capture/verify/oracle SHA；这组消除了上述早期driver版本快照限制，早期材料仍保留。capture SHA `6a93601bc3794e9ac3a764bacb80c6d45e24183a5ff4c66351276fb7ee92eb76`，verify SHA `7cfae82fa3e834495b93ae09a13ca15aba975dfd31d758d2d80a4cf0227d8435`，oracle SHA `bdc8b5327aedc4409b42db3b5cbfdbbed9ab6b53c34a41e68ef99d206f38e222`。
