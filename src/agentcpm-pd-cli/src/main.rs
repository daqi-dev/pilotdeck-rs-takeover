//! `agentcpm-pd` binary entrypoint: parses nothing itself, delegates to the
//! library so tests exercise the same code path.

fn main() {
    let code = agentcpm_pd_cli::run(std::env::args().skip(1));
    std::process::exit(code);
}
