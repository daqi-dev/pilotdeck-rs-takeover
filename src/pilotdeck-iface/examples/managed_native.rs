//! 源码构建的测试 Native 角色；不链接 resident host 或 Model HTTP 实现。
//! 与集成试件共享原 provider 源码，角色本身只使用公开 iface / Scheduler SDK。
#![forbid(unsafe_code)]

#[path = "../tests/managed_model/provider.rs"]
mod provider;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "--help") => println!("pilotdeck-iface resident_native fixture"),
        Some("--native-provider") => provider::serve(false),
        Some("--native-controller") => provider::serve(true),
        _ => {
            eprintln!("requires --native-provider or --native-controller");
            std::process::exit(64);
        }
    }
}
