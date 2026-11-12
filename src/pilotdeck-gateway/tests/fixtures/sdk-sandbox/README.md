# SDK sandbox tool-policy oracle

固定cc7c4e6 ProjectSessionRuntimeBundle.ts:775-838私有blockedToolNames及matchesToolSelector经AST定位、原函数只在内存转译/隔离VM执行；全文件源码/dist转译身份和函数hash在metadata。没有手写TS映射，也不把此边界描述成完整RuntimeBundle正式运行。

`oracle.mjs NEW_DIR`使用已装Node22.22.0，核固定Git且src/packages无改；清空环境、隔离HOME/PILOT_HOME；12policy×19name×10kind=2280格，另2个managed/explicit并集参考格。a/b双轮字节同SHA5f41ce84c5b0c268517df2be65d3d3f9f0fe1a1642c2b21c111239e34dea2887，原始_tmp/20261113-sdk-sandbox-oracle-{a,b}。

`negative-controls.mjs NEW_DIR`从真实固定函数精确token仅在内存注入4个故障，再与不可变oracle全格比较，要求全部KILLED且有首个真实反例；原源码、oracle不写。实际4/4见_tmp/20261113-sdk-sandbox-negative-a/results.json。它验证policy装置能抓错，不是Rust产品源码mutation或native运行结果。

Rust helper test另有2项真实LocalGateway+本地ControlProvider+NativeExtensions执行counter测试，模型强制call拒绝/其他会话允许/同会话下一turn保持/重启重建同policy再拒绝；目前尚未集中编译/运行。动态MCP、deferred、aliases、subagent更新过滤由统一tools接线作者验证，不因本矩阵存在而标完成。OS sandbox_mode完全独立未更改。
