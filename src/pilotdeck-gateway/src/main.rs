//! `pilotdeck-gateway`: the native gateway process on its own, without the
//! CLI in front. It is not a product deliverable (`pilotdeck server` is); it
//! stays as the test fixture that integration tests and `Options::parse`
//! callers start.
use pilotdeck_gateway::ProxyError;
#[tokio::main]
async fn main() -> Result<(), ProxyError> {
    // Node's process-wide SIGXFSZ ignore (a write past RLIMIT_FSIZE reports
    // EFBIG instead of killing the process); register the same disposition
    // at startup through tokio's safe signal API so every fs write in this
    // process matches Node (#621). The registration is held for the
    // process lifetime (a static OnceLock in the library).
    #[cfg(unix)]
    pilotdeck_gateway::ensure_sigxfsz_ignored();
    let mut args = std::env::args_os().skip(1).peekable();
    // A leading `--native` is still accepted and dropped: it used to select
    // this entry over the removed transparent proxy, and existing callers
    // still pass it. Everything else is the native options grammar.
    if args.peek().map(std::ffi::OsString::as_os_str) == Some(std::ffi::OsStr::new("--native")) {
        args.next();
    }
    pilotdeck_gateway::native_process::run(pilotdeck_gateway::native_process::Options::parse(args)?)
        .await
}
