---
id: H1-WS-DEFLATE-WINDOW-001
---

## 类别

崩溃修复：远端协商无效 permessage-deflate window 参数并发送压缩帧时，仅结束该连接，不让解压器初始化异常终止Gateway进程。

## 基线依据

固定cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85使用的Node v22.22.0原生WebSocket、内置Undici 6.23.0，真实随机loopback原始帧两轮记录在`_tmp/20261113-h1-ws-deflate/{first,second}/window-{0,16}.json`。服务端分别协商`server_max_window_bits=0`及`16`后发送压缩消息，两轮均自主exit1、signal null，stderr为`RangeError [ERR_OUT_OF_RANGE]`；栈落在原生`PerMessageDeflate.decompress`初始化InflateRaw。`double-run-compare.json`保存20格投影自比，其中这两格为真实崩溃证据。这是原生WebSocket协议层实际探针，不冒称额外正式渠道server崩溃场景。

## Rust 行为

`channel/ws_transport/deflate.rs`检查协商窗口并通过Result报告无效初始化，错误沿所属WebSocket连接收口；不panic、不终止进程，不将无效压缩消息交给渠道。正常window 8、9、15及Node实际接受的字符串09保持已录制行为。

## 范围

仅无效解压window造成进程级崩溃的边界；目前已实测输入为0和16。不豁免其它握手、RSV、mask、控制帧、分片、字典或普通错误行为；`server_no_context_takeover`仍按该Node版本实测保留字典，不能借此修改普通协议行为。

## 机器判据

`channel/ws_transport/deflate/tests.rs::native_twenty_wire_cases_preserve_messages_and_rejection`重放20格，其中window0/16返回连接错误并完成测试进程；其它有效窗口必须产生准确消息。两崩溃格的帧输入由同一确定性plain压缩重建并明确标注，stderr是捕获证据，不冒称崩溃前已采到原始帧。helper已挂载到下一候选，但截至登记未编译、未运行；正式Gateway继续存活、资源收口仍随整包验收，不由差异登记自动判绿。

## 批准记录

用户2026-11-13执行计划明确授权基线崩溃修复；根协调根据两轮真实原生进程崩溃证据批准此窄修。冻结accepted-diffs原件不变，本条不为其它未验证协议差异提供豁免。
