//! Native stdio entry point. stdout contains only Module2 NDJSON.
use pilotdeck_transport::server::ExecutionServer;
use std::io::{self, BufReader};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

fn main() -> ExitCode {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments == ["--version"] {
        println!(
            "pilotdeck-agent-loop-sidecar {} implementation=rust wire=2.0 source={}",
            env!("CARGO_PKG_VERSION"),
            env!("PILOTDECK_SOURCE_COMMIT")
        );
        return ExitCode::SUCCESS;
    }
    if !accepts_launch_arguments(&arguments) {
        eprintln!("Usage: pilotdeck-agent-loop-sidecar [--stdio | --version | <sidecar-path>]");
        return ExitCode::from(2);
    }
    // Only the Rust gateway's own built-in loop carries the in-process mark;
    // every other launch is the baseline's external loop module.
    if std::env::var_os(pilotdeck_transport::IN_PROCESS_LOOP_ENV).is_none_or(|value| value != "1") {
        pilotdeck_transport::serve_as_external_loop();
    }
    if std::env::var_os("PILOTDECK_AGENT_LOOP_FACTORY").is_some_and(|value| !value.is_empty()) {
        eprintln!(
            "PILOTDECK_AGENT_LOOP_FACTORY (a JavaScript agent loop factory module) is not supported by the Rust build of PilotDeck."
        );
        return ExitCode::from(2);
    }
    // pilotdeck-agent-loop-sidecar.ts:27-46: a TCP port selects the
    // long-lived listener; an empty value keeps stdio.
    let tcp_port = match optional_port(std::env::var("PILOTDECK_AGENT_LOOP_TCP_PORT").ok()) {
        Ok(port) => port,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(1);
        }
    };
    // #1371: an armed controlled loop resolves and validates its plan before
    // the server advertises readiness, so an invalid control directory fails
    // closed here exactly like the gateway assembly.
    if let Ok(value) = std::env::var(pilotdeck_transport::loop_stimulus::STIMULUS_ENV) {
        match pilotdeck_transport::loop_stimulus::validate_value(&value) {
            Ok(dir) => {
                // Resolve and freeze the plan here, at startup, not on the
                // first turn: the gateway resolved its own copy before it
                // launched this process, so a lazy read would let the two
                // disagree if `loop.json` changes in between and silently
                // record an uncontrolled turn under a planned cell.
                match pilotdeck_transport::loop_stimulus::LoopStimulus::global() {
                    Some(stimulus) => {
                        if let Err(message) = stimulus.plan() {
                            eprintln!("{message}");
                            return ExitCode::from(2);
                        }
                    }
                    // The entry above only exists when the variable is set.
                    None => {
                        eprintln!(
                            "native loop stimulus could not resolve {}.",
                            pilotdeck_transport::loop_stimulus::STIMULUS_ENV
                        );
                        return ExitCode::from(2);
                    }
                }
                eprintln!("native loop stimulus on {}", dir.display());
            }
            Err(message) => {
                eprintln!("{message}");
                return ExitCode::from(2);
            }
        }
    }
    let process_cwd = match std::env::current_dir().and_then(|path| {
        path.into_os_string().into_string().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "Process cwd is not valid UTF8.")
        })
    }) {
        Ok(cwd) => cwd.into(),
        Err(error) => {
            eprintln!("Sidecar initialization failed: {error}");
            return ExitCode::from(2);
        }
    };
    // agentLoopSidecarServer.ts:101-102, 621: one instance per process; its
    // connections share one operation table (84-86).
    let connection = pilotdeck_transport::hosted::instance(process_cwd);
    if let Some(port) = tcp_port {
        let host = std::env::var("PILOTDECK_AGENT_LOOP_TCP_HOST")
            .ok()
            .map(|host| host.trim().to_owned())
            .filter(|host| !host.is_empty())
            .unwrap_or_else(|| "127.0.0.1".into());
        pilotdeck_transport::serve_as_tcp_sidecar();
        return serve_tcp(&host, port, connection);
    }
    let server = connection();
    let report = server.serve(
        BufReader::new(io::stdin()),
        io::stdout(),
        pilotdeck_transport::hosted::SERVE_GRACE,
    );
    if report.stalled {
        // pilotdeck-agent-loop-sidecar.ts:29 awaits sidecar.serve(), which
        // awaits every execution (agentLoopSidecarServer.ts:205). With input
        // ended and an execution pending on a host reply that can no longer
        // come, Node's event loop empties and the process ends with exit code
        // 13 (unsettled top-level await). Node's warning on stderr names its
        // own script location and is not reproduced.
        ExitCode::from(13)
    } else if report.complete() {
        ExitCode::SUCCESS
    } else {
        // The OS process is the final supervisor for a stuck input/output/host
        // worker. Do not report graceful thread cleanup which did not happen.
        eprintln!(
            "Sidecar shutdown incomplete: reader_joined={} writer_joined={} worker_joined={}",
            report.reader_joined, report.writer_joined, report.worker_joined
        );
        ExitCode::from(3)
    }
}

/// `AgentLoopSidecarTcpServer` (tcpAgentLoopSidecarServer.ts:23-80): one
/// long-lived listener; every accepted socket is served as its own
/// connection of this sidecar instance (same module instance, a fresh
/// connection generation) until the process is stopped. A connection that
/// goes away retires only itself.
fn serve_tcp(host: &str, port: u16, connection: impl Fn() -> Arc<ExecutionServer>) -> ExitCode {
    let listener = match std::net::TcpListener::bind((host, port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("Sidecar TCP listen failed: {error}");
            return ExitCode::from(1);
        }
    };
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            // A failing accept (file descriptors exhausted, …) fails again
            // at once while the peer waits in the backlog: back off instead
            // of spinning. Like the baseline's listener (libuv drops the
            // pending peer on EMFILE) it is silent and keeps listening.
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        let _ = stream.set_nodelay(true);
        let (Ok(input), Ok(output)) = (stream.try_clone(), stream.try_clone()) else {
            continue;
        };
        let server = connection();
        std::thread::spawn(move || {
            let _ = server.serve(
                BufReader::new(input),
                output,
                pilotdeck_transport::hosted::SERVE_GRACE,
            );
            // Threads still blocked after the grace period keep their
            // clones; close the socket itself so the host sees the end.
            let _ = stream.shutdown(std::net::Shutdown::Both);
        });
    }
    ExitCode::SUCCESS
}

/// `optionalPort` (pilotdeck-agent-loop-sidecar.ts:39-46): JS `Number()` of
/// the trimmed text, an integer in 1..=65535; empty or unset means stdio.
fn optional_port(value: Option<String>) -> Result<Option<u16>, String> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    let text = value.trim();
    let radix = |prefix: &str, radix| {
        text.strip_prefix(prefix)
            .or_else(|| text.strip_prefix(&prefix.to_uppercase()))
            .map(|digits| u64::from_str_radix(digits, radix).ok().map(|n| n as f64))
    };
    let number = radix("0x", 16)
        .or_else(|| radix("0o", 8))
        .or_else(|| radix("0b", 2))
        .unwrap_or_else(|| {
            text.parse::<f64>().ok().filter(|_| {
                !text.contains(|c: char| c.is_ascii_alphabetic() && c != 'e' && c != 'E')
            })
        });
    match number {
        Some(port) if port.fract() == 0.0 && (1.0..=65_535.0).contains(&port) => {
            Ok(Some(port as u16))
        }
        _ => Err("PILOTDECK_AGENT_LOOP_TCP_PORT must be an integer between 1 and 65535.".into()),
    }
}

// A single nonempty path is only a launch placeholder for the TS gateway;
// it is ignored, and every accepted invocation uses stdio.
fn accepts_launch_arguments(arguments: &[String]) -> bool {
    arguments.is_empty()
        || (arguments.len() == 1
            && !arguments[0].is_empty()
            && (arguments[0] == "--stdio" || !arguments[0].starts_with("--")))
}

#[cfg(test)]
mod tests {
    use super::accepts_launch_arguments;

    #[test]
    fn stdio_accepts_one_ignored_path_and_rejects_invalid_arguments() {
        for arguments in [
            vec![],
            vec!["--stdio"],
            vec!["/tmp/PilotDeck/dist/agent-loop-sidecar.js"],
            vec!["relative-sidecar.js"],
            vec!["-sidecar.js"],
        ] {
            let arguments = arguments.into_iter().map(String::from).collect::<Vec<_>>();
            assert!(accepts_launch_arguments(&arguments), "{arguments:?}");
        }
        for arguments in [
            vec![""],
            vec!["--unknown"],
            vec!["--stdio", "sidecar.js"],
            vec!["sidecar.js", "another.js"],
            vec!["--version", "sidecar.js"],
        ] {
            let arguments = arguments.into_iter().map(String::from).collect::<Vec<_>>();
            assert!(!accepts_launch_arguments(&arguments), "{arguments:?}");
        }
    }
}
