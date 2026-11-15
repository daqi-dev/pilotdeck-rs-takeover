//! 源码构建的测试 Native 角色；不链接 resident host 或 Model HTTP 实现。
//! 与集成试件共享原 provider 源码，角色本身只使用公开 iface / Scheduler SDK。
#![forbid(unsafe_code)]

#[path = "../tests/managed_model/provider.rs"]
mod provider;
#[path = "managed_native/tool_fixture.rs"]
mod tool_fixture;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "--help") => println!("pilotdeck-iface resident_native fixture"),
        Some("--native-provider") => provider::serve(false, false),
        Some("--native-provider-demand") => provider::serve(false, true),
        Some("--native-controller") => provider::serve(true, false),
        Some("--p2-tool-fixture") => tool_fixture::serve(),
        Some("--p2-product-controller") => {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    agentcpm_scheduler_protocol::provider::serve(
                        agentcpm_scheduler_protocol::provider::Export::Controller(
                            std::sync::Arc::new(pilotdeck_iface::product::ProductController {
                                loop_binding: "pilotdeck".into(),
                                controller_binding: "controller".into(),
                            }),
                        ),
                    )
                    .await
                    .unwrap();
                });
        }
        _ => {
            eprintln!("requires --native-provider or --native-controller");
            std::process::exit(64);
        }
    }
}
