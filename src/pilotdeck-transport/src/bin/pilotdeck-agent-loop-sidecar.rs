//! Native stdio/TCP entry point. stdout contains only Module2 NDJSON in stdio.
use pilotdeck_protocol::admission::SidecarBinding;
use pilotdeck_transport::default_factory::OriginalAgentFactory;
use pilotdeck_transport::server::ExecutionServer;
use pilotdeck_transport::stdio::ConnectionOwner;
use pilotdeck_transport::system_clock::SystemExecutionClock;
use pilotdeck_transport::tcp::{TcpExecutionServer, TcpServerOptions};
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
    if std::env::var_os("PILOTDECK_AGENT_LOOP_FACTORY").is_some_and(|value| !value.is_empty()) {
        eprintln!(
            "Rust candidate does not yet support the PILOTDECK_AGENT_LOOP_FACTORY deployment profile."
        );
        return ExitCode::from(2);
    }
    let tcp_port = match std::env::var("PILOTDECK_AGENT_LOOP_TCP_PORT") {
        Ok(value) => match optional_tcp_port(&value) {
            Ok(port) => port,
            Err(message) => {
                eprintln!("{message}");
                return ExitCode::FAILURE;
            }
        },
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => {
            eprintln!("PILOTDECK_AGENT_LOOP_TCP_PORT must be an integer between 1 and 65535.");
            return ExitCode::FAILURE;
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
    let module_instance = format!("pilotdeck-agent-loop-instance-{}", uuid::Uuid::new_v4());
    let binding = SidecarBinding {
        module_id: "pilotdeck-agent-loop".into(),
        module_instance_id: module_instance.clone().into(),
        connection_generation: format!("{module_instance}-{}", uuid::Uuid::new_v4()).into(),
        capabilities_version: "1".into(),
    };
    let clock = Arc::new(SystemExecutionClock);
    let factory = Arc::new(
        OriginalAgentFactory {
            clock: clock.clone(),
            process_cwd,
        }
        .sidecar_public(),
    );
    if let Some(port) = tcp_port {
        let host = std::env::var("PILOTDECK_AGENT_LOOP_TCP_HOST")
            .ok()
            .map(|value| value.trim_matches(js_whitespace).to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "127.0.0.1".into());
        let server = match TcpExecutionServer::listen(
            TcpServerOptions {
                host,
                port,
                ..Default::default()
            },
            factory,
            clock,
        ) {
            Ok(server) => server,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        };
        return match server.wait() {
            Ok(report) if report.complete() => ExitCode::SUCCESS,
            Ok(report) => {
                eprintln!("Sidecar TCP shutdown incomplete: {report:?}");
                ExitCode::from(3)
            }
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        };
    }
    let server = ExecutionServer::new(ConnectionOwner::new(binding, 64), factory, clock, 32);
    let report = server.serve(
        BufReader::new(io::stdin()),
        io::stdout(),
        Duration::from_secs(5),
    );
    if report.complete() {
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

// A single nonempty path is only a launch placeholder for the TS gateway;
// it is ignored; deployment is selected by the environment like the baseline.
fn accepts_launch_arguments(arguments: &[String]) -> bool {
    arguments.is_empty()
        || (arguments.len() == 1
            && !arguments[0].is_empty()
            && (arguments[0] == "--stdio" || !arguments[0].starts_with("--")))
}

fn optional_tcp_port(value: &str) -> Result<Option<u16>, &'static str> {
    let value = value.trim_matches(js_whitespace);
    if value.is_empty() {
        return Ok(None);
    }
    let number = if let Some((digits, radix)) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map(|digits| (digits, 16))
        .or_else(|| {
            value
                .strip_prefix("0o")
                .or_else(|| value.strip_prefix("0O"))
                .map(|digits| (digits, 8))
        })
        .or_else(|| {
            value
                .strip_prefix("0b")
                .or_else(|| value.strip_prefix("0B"))
                .map(|digits| (digits, 2))
        }) {
        (!digits.is_empty() && digits.chars().all(|value| value.is_digit(radix)))
            .then(|| u32::from_str_radix(digits, radix).ok().map(f64::from))
            .flatten()
    } else {
        value.parse::<f64>().ok()
    };
    number
        .filter(|number| {
            number.is_finite() && number.fract() == 0.0 && (1.0..=65_535.0).contains(number)
        })
        .map(|number| Some(number as u16))
        .ok_or("PILOTDECK_AGENT_LOOP_TCP_PORT must be an integer between 1 and 65535.")
}
fn js_whitespace(value: char) -> bool {
    matches!(value, '\u{0009}'..='\u{000D}' | '\u{0020}' | '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}')
}

#[cfg(test)]
mod tests {
    use super::accepts_launch_arguments;

    #[test]
    fn tcp_port_matches_javascript_number_and_empty_environment_selection() {
        for value in ["", " \t\n", "\u{feff}"] {
            assert_eq!(super::optional_tcp_port(value), Ok(None));
        }
        for (value, port) in [
            (" +123.0 ", 123),
            ("1e3", 1000),
            ("0x1a", 26),
            ("0O77", 63),
            ("0b101", 5),
            ("65535", 65535),
        ] {
            assert_eq!(super::optional_tcp_port(value), Ok(Some(port)), "{value}");
        }
        for value in [
            "0", "65536", "1.1", "NaN", "Infinity", "-2", "2px", "0x", "-0xff", "0x+1", "0b+1",
        ] {
            assert!(super::optional_tcp_port(value).is_err(), "{value}");
        }
    }

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
