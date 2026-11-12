# T1 PDF 旧全量失败接手审查

2026-11-13；根候选 `e8618392e7e4eb24c227eedd28e14c84237ca8a4`。只读审查，未运行 Cargo、TS 或产品，未修改测试和产品。旧全量 `ea6d8e71` 的 `builtin_tool_outputs_match_real_ts`、`multimodal_success_branches_emit_model_visible_payloads` 首次均因 PDF component unavailable 失败；证据与身份见 `docs/backend-delivery/E0-resume.md`。这个结果不能单独证明真实 helper 下坏文件会怎样。

固定 TS `src/tool/builtin/filesystem/fileTypeSafety.ts:169-176` 对打开或计页失败 catch 并返回 undefined；`src/tool/builtin/readFile.ts:253-266,279-287,337-355,416-440` 随后使用可选页数：无 PDF/image 能力时返回说明，PDF 直传路径将原始字节放进 supplemental 内容。现存 `tests/fixtures/builtin-tools/generate.mts:343-349,360-364` 的坏 PDF 场景及 `tests/builtin_tools.rs:204-227` 模型可见字节断言有明确基线依据；不能仅为通过把它们替换成合法 PDF。`read_file/pdf.rs:81-82` 对 `document.pages()` 所有错误直接返回，源码显示与上述降级路径不一致；带真实 helper 的实际结果仍需复跑确认。未找到明确授权将坏 PDF 计数失败变成工具错误的 T1 差异。

建议根分配产品修改权后窄修：区分组件不可用（本轮发行要求明确报告）与实际解析/计页失败（按 TS 使用未知页数继续），各分支有无页数决定输出字段及文案；图像渲染路径仍应使用真实渲染结果，不把任意故障伪装成成功。对未知页数下的 pages 范围和计数为零也要按基线核对，不以 `0` 代替 undefined。保留旧坏输入 oracle；增加真实合法 PDF 的成功页数及模型 payload 断言，坏输入的无页数直传和无能力降级断言，以及缺组件单独断言。必要时先由根排轻量 TS 槽验证具体错误序列，不能从组件底层 6 项直接推断正式工具输出。

E0 已收到请求：根统一排程后在固定源码、真实 MuPDF helper 下重跑两项，保存组件、二进制和命令身份，保留首败。此审查不把推断写成实测，不代表 T1 或全量门禁完成。

## 修复候选与新证据

E0 在固定 `e8618392`、真实 helper SHA `bf370bfe6463d644b29f466b01ad3ac8bbac194fdb53bed860cfeb6e88ee69cb` 下重跑旧两项，2 run / 0 pass / 2 fail；二者均由计页 helper 对坏 PDF 失败引起，原始日志 `logs/e8618392-environment-replay/pdf.log`（cb8；E0 负责同步）。旧组件缺失首败保留。

本机隔离临时目录直接调用固定 TS 的 `createReadFileTool`，Node 22.22.0，生成一页合法 PDF 和原 9 字节坏 PDF；两轮八格 `observed.json` SHA 均为 `4df030ca21c663a58795d1d1084a3435d517b9bf72b492612f1275cc49a86781`，驱动 SHA `9d4ed874dfaa8e2210e71627ff81cbb8a3302eca85b904bf678a7683b1223bfb`，见 `_tmp/20261113-t1-resume/`。坏 PDF 无能力输出或 PDF 直传成功，JSON 省略未知页数字段，TS 实际对象保留值为 undefined 的自有键；image 分支运行 render 后报告 `no objects found`。合法一页两种模式均显示页数 1，指定第 2 页返回越界错误。该取证不等同完整 TS 自比及产品全量验收。

候选 `read_file/pdf.rs` 对 `pages()` 成功零页或 `Failed` 使用未知页数；`Unavailable`、`Timeout`、`TooLarge` 仍明确报错。未知页数保留 JsValue::Undefined，越界与 10 页门只在页数已知时生效，渲染仍执行真实 helper，默认最多 20 页；不替换旧坏 PDF oracle。新增分类单测、坏 PDF 渲染失败检查和自制合法一页 PDF 的真实模型 payload 测试，并加强坏 PDF 未知页数断言。helper 的 Failed 同时包含解析和部分进程失败，当前无法精细区分；渲染 helper 的原始错误与 TS MuPDF 文案可能不同，须以真实组件正式比对记录，不能以非空错误断言证明严格文案一致。

仅执行了两个文件的 rustfmt 和 `git diff --check`，均成功；未执行本机 Cargo。待独立审阅、固定新源码后由 E0 在真实 helper 下跑原两项、新增集成与私有测试，并执行正式入口比对。用户规定缺组件明确报错与 TS 动态 import 失败时降级之间的差异应在发行验收中明确记录，不以此次测试通过替代。

## 独审后的错误分类窄修

根独审指出先前把所有 `Failed` 降级会吞掉启动、读取和取消等故障；上述初版分类与限制已被本段候选取代。`native_component::Error::Exited` 保留真实退出码和既有错误文字；仅 count 子进程正常退出码 1 转为 `ParseFailed` 并降级未知页数。启动 I/O、退出码 2/127、信号、取消、非法计数输出、超时和输出超限继续报错。image 对新退出类型仍输出原文字。helper 源码中参数错误为 2、MuPDF 异常为 1；未改 helper。

新增子进程测试针对不可执行文件、信号与 exit 1，分类测试覆盖 count/render、退出码 1/2/127/信号和取消；现有 waiter drop 测试检查进程回收。坏 PDF/合法 PDF 仍走真实 helper 的集成测试。修后 rustfmt 与 diff check 成功；未运行 Cargo，新候选必须由 E0 固定源码/组件身份后执行全部相关测试。退出码 1 包括 helper 自身 MuPDF 初始化失败，这是 helper 既有协议的粒度边界，不能从退出码进一步区分。
