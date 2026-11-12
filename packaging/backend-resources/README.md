# 后端发行资源候选

这是独立编写的产品资源，不是从固定 TypeScript 参考树复制的发行物。32 个顶层 skill 名称与 3 个 builtin plugin 均列在 `RESOURCE-COVERAGE.json`。本期产品前端实现排除不影响模型使用 frontend 领域创作技能。本目录不是 32 项能力等价完成声明；高级 Office 工具、外部服务与平台相关缺口逐项保留。

资源正文和新增脚本采用本仓库 AGPL-3.0-only，LICENSE 随 skills 和 builtin-plugins 两个输入根携带。未收录第三方代码、字体、模型、Node、浏览器或 Python 包；其版本、来源和许可证须由实际发行布局另行记录。不存在从参考目录复制文本、脚本或第三方许可证的步骤。

## 可实际运行的内容

- `skills/_tools/artifact_inspect.py`：Python 标准库只读检查 OOXML/CSV/TSV，包括文本和公式清单、实体攻击拒绝、输入上限；不宣称渲染或公式计算。
- `skills/_tools/artifact_build.py`：利用单独安装的 python-docx/openpyxl/python-pptx，从小型 JSON spec 创建真正可编辑的 DOCX/XLSX/PPTX。保护已有输出；不提供完整排版或高级 redline。
- `skills/pdf/scripts/pdf_pages.py`：利用单独安装的 pypdf 检查页框/表单字段名、选页/重排/旋转。保护输入；不渲染、不为结构操作保证完整交互表单目录保留。
- 技能迁移 helper：只接受显式单个源，拒绝覆盖或 symlink，复制后比对字节清单；源须在运行期间保持稳定。它不搜索用户目录、不替代正式 loader 验证。
- SRT helper：把现有字幕转换为保留时标的结构化 transcript；不录音、不推断说话人、不伪造纪要。
- FunASR MCP：独立 Node `.mjs` stdio 服务，真实调用单独源码构建的 SenseVoice CLI。受 project-root 路径约束，缺 runtime/model 报错，支持请求取消及等待实际子进程 close。测试仅使用临时 mock executable，不冒充声学识别。
- browser-use plugin：通过现有宿主 marker 接入独立外部 Playwright MCP；没有伪造工具服务。实际依赖未提供时启动必须按真实错误报告。

运行本地资源验证：

```sh
python3 -B -m unittest discover -s packaging/backend-resources/tests -v
node --check packaging/backend-resources/builtin-plugins/funasr/funasr-local-mcp.mjs
```

测试环境需要上述 Python 格式库；这些库没有被本包安装或打包。MCP subprocess 用临时 HOME/PILOT_HOME，仅访问临时 mock 文件，没有真实模型或账号。13项通过只属于资源脚本，不代替 Rust/正式 CLI/发行验收。

## 发行输入与真实 loader 测试方案

组包使用已经存在的 `scripts/backend-release/assemble.py` 输入：

```text
--skills packaging/backend-resources/skills
--plugins packaging/backend-resources/builtin-plugins
```

不要因为两个目录存在就移除发行 pendingDelivery。根资源覆盖账本也必须随最终发行证据保留；当前组包器仅复制上述两个输入根，不会自动携带本目录顶层的覆盖账本。

真实 loader验收须由根/E0在固定源码与二进制执行，保存原始结果和产物哈希：

1. **装配。** 将候选 CLI 安装到独立 `bin/pilotdeck`，资源根位于相邻 `skills` 与 `builtin-plugins`；从包外工作目录启动，隔离 HOME/PILOT_HOME，禁止挂载 TS 参考树。也可使用现有 `PILOTDECK_BUNDLED_SKILLS_DIR`、`PILOTDECK_BUNDLED_PLUGINS_DIR` 先作定点检查。正式搬迁验收必须移除这两个 override。
2. **发现/提示。** 对正式 `commands_list` 和 `skill_list` 使用仓库既有请求格式，断言32个skill名称全部出现、来源为builtin；查询windows-skills内的powershell-safe-invocation贡献。通过真实 skill prompt加载能力检查DOCX的 `{{SKILL_ROOT_SHELL}}` 已替换为搬迁后的安全引号路径，且按该路径运行helper成功。不能只对SKILL.md全文grep当加载成功。
3. **优先级。** 在隔离project/global目录放同名测试skill，确认project > global > builtin；移除覆盖后builtin恢复。所有fixture均独立编写，不更改发行资源来制造成功。
4. **禁用。** 设置 `extension.builtinPluginsEnabled` 分别禁用 browser-use/funasr/windows-skills，检查贡献和MCP子进程确实消失；恢复后重新发现。通过项目 `modules.skills.enabled: false` 检查技能路径按现有约定禁用。请依据现有wire fixture选择实际接口，不发明新字段。
5. **真实MCP边界。** 在隔离 runtime tree使用已测试mock SenseVoice executable，正式会话发现 `mcp__funasr__transcribe_audio` 并调用；校验mock transcript/segments和实际进程退出。缺失模型、audio symlink逃逸、取消以及stop时正在运行的调用均应返回对应失败并清理实际子进程。随后才安排已源码构建引擎和合许可模型的真实音频验收。
6. **browser-use。** 记录独立Playwright MCP源码revision、lock、Node、browser身份和许可。`PILOTDECK_RUNTIME_ROOT` 可定位外部root下 `node_modules/@playwright/mcp/cli.js`。正式会话列出真正工具，打开localhost fixture、读snapshot、执行一次输入并观测DOM变化；缺依赖时不能用空tools成功代替。当前资源没有完成这个服务的离线发行。
7. **负控。** 在独立资源副本改掉一个skill名称/删掉必须helper，真实loader/调用校验须失败；禁用插件副本却仍出现工具也必须失败。保留首次失败，恢复未改副本再验证。复制资源用于测试不等于复制参考树。

相关实现依据：`pilotdeck-cli/src/skills.rs` 与 `server.rs` 选择安装布局；`runtime/plugins/loader.rs` 发现builtin/global/project；`loader/markdown.rs`读frontmatter与SKILL；`contributions.rs`替换skill-root并选择优先级；`runtime/mcp/registry.rs`按plugin id替换现有MCP marker。未修改这些正在冻结的Rust文件。

## 外部引擎来源

[官方 FunASR native CLI说明](https://github.com/modelscope/FunASR/blob/main/runtime/llama.cpp/README.md) 和 [CLI参数实现](https://github.com/modelscope/FunASR/blob/main/runtime/llama.cpp/sensevoice/funasr-sensevoice/funasr-sensevoice.cpp)支持SenseVoice、VAD和SRT调用。这里仅核验接口，不把浮动main当固定发行来源；实际构建必须固定commit。runtime路径沿用宿主已有布局：`<pilot-home>/funasr/runtime/v0.2.0/<platform>-<arch>/`，模型位于 `models/sensevoice-small-q8.gguf` 和 `models/fsmn-vad.gguf`。

[Microsoft Playwright MCP官方源](https://github.com/microsoft/playwright-mcp)说明外部MCP服务和browser选择。未自动执行安装、未下载预编译浏览器、未使用真实凭据；缺少固定source/runtime/license与实跑证据仍是未交付项。

## 本轮验证记录

本机10项资源脚本测试通过，34项skill frontmatter校验通过。独立资源副本将真实MCP transcript改为错误文本后，专用进程测试以内容不匹配退出1；没有修改正式候选以制造通过。证据在 `_tmp/20261113-d1-resources/`，包含负控日志、静态校验结果和候选文件哈希。后续文档/覆盖账本补充不改变脚本测试结论；最终发行仍须重新记录实际打包文件哈希。FunASR异常/缺依赖文案尚未实现固定基线逐字等价，账本明确列为未闭合，不能视为已接受差异。

## 独审确认的 owned 清理与工作表重名修复

首轮独审真实probe证明，直接kill native child会留下继承stdout/stderr的后代，取消没有receipt、EOF也不退出；旧证据保存在 `_tmp/20261113-resource-review/`。当前FunASR扩展增加Python3 POSIX supervisor（随plugin的supervise.py），不增加核心Rust依赖。Node以独立session启动它；它保持group leader存活，通过不传给native的fd3报告真实native退出。正常完成、取消、输出限制或EOF均先通知leader，由leader对自己的仍owned group作最终SIGKILL，再等待真实stdio close。Node不会在leader已reap后按其PID/PGID补杀，也不按进程名称清理。控制stdin EOF或Python异常同样触发owned group结束；缺Python和native启动失败有实际错误receipt。

该安全边界只覆盖同一owned POSIX group，不声称约束主动创建新session逃逸的恶意可执行文件；native引擎来源必须受控。Windows没有可靠job-object实现，当前明确返回能力不可用，不能将taskkill或直接child kill当作已实现的子树释放。POSIX macOS/Linux需要可执行Python3，Node与引擎模型仍是可选扩展外部依赖。真实引擎/Windows均未完成发行验收。

XLSX创建在任何写入前按casefold拒绝重复sheet名；创建后再次比对actualname，防止库静默把Data/data改为Data/data1。正控重新打开Data/Other验证名字保持，反例要求失败且无输出文件。

新13项资源测试通过，其中owned进程测试覆盖cancel、EOF、native正常结束但后代持pipe三组，确认native没有继承statusfd且leader/native/后代exact PID均消失；缺Python/native启动失败也有界返回真实错误。独立副本禁用group清理和恢复case-sensitive silentrename两个负控均须被拒绝，证据在 `_tmp/20261113-resource-owned-fix/`。前述初轮10项/34项记录保留为旧证据，不能冒充本次或真实loader验证。
