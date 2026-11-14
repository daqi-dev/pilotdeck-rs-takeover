# 独立 Rust 原生扩展消费者

本 crate 使用 `pilotdeck_gateway::extensions` 的正式公开接口；它有自己的 Cargo workspace 和锁文件，不是 gateway 的测试或 workspace 成员。依赖来自同一源码发行布局中的 `../../src/`，无需发布到 crates.io，也无需 Node 或 sidecar 可执行文件。

将源码包解压为一个新目录，将本示例放在该目录的 `examples/native-extension-consumer/`，保留相邻 `src/` 和根 `LICENSE`。使用源码包指定工具链，从源码根运行：

```sh
cargo +1.99 run --locked --offline --release --manifest-path examples/native-extension-consumer/Cargo.toml
```

编译依赖需已经在 Cargo 缓存中，或由用户另行准备对应锁定 crates 的源码；源码发行的依赖许可证清单继续适用。本例直接依赖的 Rust 公共 crate、serde_json 和 uuid 均已在产品依赖闭包中，不增加外部依赖。运行只监听动态 loopback 端口，创建独占临时 HOME、PILOT_HOME 和项目；不读取用户配置或真实 provider 凭证。完成后关闭 gateway、join mock server，并删除自有临时目录。

程序将 read-only 工具、提示、包装正常 native 执行器的 factory 和 Drop 资源作为一代扩展注册。真实 mock provider 从收到的模型请求中看见工具并返回 tool call，正式工具权限/dispatch 完成调用。更新后旧 session 继续使用第一代，新 session 使用第二代；旧 token 的 drop 不移除新版。卸载第二代后新 session 不再向模型暴露工具/提示，最后 gateway shutdown 释放两代资源。程序逐项断言实际 provider 请求、调用代际和释放计数，成功输出单行 JSON；失败以非零退出。

该示例不承诺动态库 ABI、JS 插件加载器、任意 router 矩阵或所有取消/恢复路径；这些属于正式 API 的其他合同。本例不修改任何产品 API。
