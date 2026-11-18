//! Production scoped Native Loop and Product Controller binary entry point.
#![forbid(unsafe_code)]

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect::<Result<Vec<_>, _>>();
    let outcome = match args {
        Ok(args) => pilotdeck_iface::scoped_native_entry::dispatch_native_loop_cli(&args).await,
        Err(_) => Err(
            pilotdeck_iface::scoped_native_entry::NativeLoopCliError::Usage(
                "arguments must be valid UTF-8".into(),
            ),
        ),
    };
    if let Err(error) = outcome {
        eprintln!("{error}");
        std::process::exit(error.exit_code());
    }
}
