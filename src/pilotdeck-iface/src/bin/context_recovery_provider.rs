//! Dedicated, explicitly configured Native Recovery provider entry point.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect::<Result<Vec<_>, _>>();
    let outcome = match args {
        Ok(args) => pilotdeck_iface::recovery_launch::dispatch_recovery(&args).await,
        Err(_) => Err(agentcpm_scheduler_protocol::Failure::external(
            "CONTEXT_RECOVERY_CONFIG",
            "arguments must be UTF-8",
        )),
    };
    match outcome {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("missing Recovery provider configuration");
            std::process::exit(2);
        }
        Err(error) => {
            eprintln!("{}", error.wire());
            std::process::exit(1);
        }
    }
}
