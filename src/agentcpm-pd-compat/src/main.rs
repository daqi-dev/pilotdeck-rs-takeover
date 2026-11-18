//! `pilotdeck` compat shim binary entrypoint: parses nothing itself, delegates
//! to the library so tests exercise the same code path (same shape as the
//! `agentcpm-pd` binary in `agentcpm-pd-cli`).

fn main() {
    let code = agentcpm_pd_compat::run(std::env::args().skip(1));
    std::process::exit(code);
}
