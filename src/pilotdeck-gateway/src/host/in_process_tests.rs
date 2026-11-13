use super::*;
use crate::host::{HostClient, HostErrorKind, Limits, connection::Connection};
use std::io::{BufRead, Read, Write};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn limits() -> Limits {
    Limits {
        handshake_timeout: Some(Duration::from_secs(30)),
        close_timeout: Duration::from_millis(300),
        ..Limits::default()
    }
}

#[tokio::test]
async fn pipes_carry_bytes_both_ways_and_report_either_end_closing() {
    // Sync writer -> async reader, EOF once the writer is gone.
    let (mut writer, mut reader) = pipe();
    let thread = std::thread::spawn(move || {
        Write::write_all(&mut writer, b"one\n").unwrap();
        Write::write_all(&mut writer, b"two\n").unwrap();
    });
    let mut text = String::new();
    AsyncReadExt::read_to_string(&mut reader, &mut text)
        .await
        .unwrap();
    thread.join().unwrap();
    assert_eq!(text, "one\ntwo\n");

    // Async writer -> sync reader, EOF on shutdown.
    let (mut writer, reader) = pipe();
    let thread = std::thread::spawn(move || {
        let mut lines = Vec::new();
        for line in std::io::BufReader::new(reader).lines() {
            lines.push(line.unwrap());
        }
        lines
    });
    AsyncWriteExt::write_all(&mut writer, b"a\nb\n")
        .await
        .unwrap();
    AsyncWriteExt::shutdown(&mut writer).await.unwrap();
    assert_eq!(thread.join().unwrap(), ["a", "b"]);

    // A writer whose reader is gone fails like a broken pipe, blocking or not.
    let (mut writer, reader) = pipe();
    drop(reader);
    assert_eq!(
        Write::write(&mut writer, b"x").unwrap_err().kind(),
        std::io::ErrorKind::BrokenPipe
    );
    assert_eq!(
        AsyncWriteExt::write_all(&mut writer, b"x")
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::BrokenPipe
    );
}

#[tokio::test]
async fn a_full_pipe_holds_its_writer_until_the_reader_drains_it() {
    let (mut writer, mut reader) = pipe();
    let total = PIPE_CAPACITY * 3 + 17;
    let thread = std::thread::spawn(move || {
        Write::write_all(&mut writer, &vec![7u8; total]).unwrap();
    });
    // The writer cannot finish before anything is read.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!thread.is_finished());
    let mut received = Vec::new();
    AsyncReadExt::read_to_end(&mut reader, &mut received)
        .await
        .unwrap();
    thread.join().unwrap();
    assert_eq!(received.len(), total);

    // And the other way round: a blocking reader wakes for an async writer.
    let (mut writer, mut reader) = pipe();
    let thread = std::thread::spawn(move || {
        let mut received = Vec::new();
        Read::read_to_end(&mut reader, &mut received).unwrap();
        received.len()
    });
    AsyncWriteExt::write_all(&mut writer, &vec![1u8; total])
        .await
        .unwrap();
    drop(writer);
    assert_eq!(thread.join().unwrap(), total);
}

/// The built-in loop is served on a thread of this process: the handshake
/// reaches the real sidecar server, and no child process exists.
#[tokio::test]
async fn the_built_in_loop_handshakes_without_a_child_process() {
    let cwd = std::env::temp_dir();
    let client = HostClient::in_process(&cwd, Some(&"pilotdeck-agent-loop".into()), limits())
        .await
        .unwrap();
    assert!(
        client
            .binding()
            .module_id
            .equals_str("pilotdeck-agent-loop")
    );
    assert_eq!(client.child_id(), None);
    let started = Instant::now();
    client.close().await.unwrap();
    // Closing the input ends the server: no bound had to expire.
    assert!(started.elapsed() < Duration::from_millis(300));
}

/// A panic on the loop's thread ends this connection like a child that died:
/// the host sees its output close, and the gateway process carries on.
#[tokio::test]
async fn a_panicking_loop_fails_its_connection_not_the_gateway() {
    let launched = launch_with(|mut input, _output| {
        let mut line = String::new();
        let _ = input.read_line(&mut line);
        panic!("loop panicked after {line:?}");
    })
    .unwrap();
    let mut connection = Connection::in_process(launched, limits());
    let mut frame = pilotdeck_protocol::JsObject::new();
    frame.insert("kind".into(), "request".into());
    connection
        .send(&pilotdeck_protocol::JsValue::Object(frame))
        .unwrap();
    let Err(error) = connection.receive().await else {
        panic!("output closed")
    };
    assert_eq!(error.kind, HostErrorKind::Disconnected);
    connection.close().await.unwrap();
    assert_eq!(
        connection.exit(),
        Some(crate::host::SidecarExit::Code(Some(101)))
    );
    // A second panicking loop on a later turn is just as contained.
    let launched = launch_with(|_, _| panic!("again")).unwrap();
    assert_eq!(launched.ended.await.unwrap(), LoopEnd::Panicked);
}

/// A loop that never ends cannot hold the turn: close gives up on it after
/// its bound and leaves the thread behind, with both pipes closed.
#[tokio::test]
async fn a_stuck_loop_is_left_behind_after_the_close_bound() {
    let (release, released) = std::sync::mpsc::channel::<()>();
    let launched = launch_with(move |_, _| {
        let _ = released.recv();
        LoopEnd::Served {
            complete: true,
            stalled: false,
        }
    })
    .unwrap();
    let mut connection = Connection::in_process(launched, limits());
    let started = Instant::now();
    connection.close().await.unwrap();
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(300), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");
    assert_eq!(connection.exit(), None);
    drop(release);
}
