//! A loopback port that refuses connections for the rest of the test
//! process.
//!
//! Binding a listener to port 0, reading its port and closing it again only
//! names a port that is free *now*: another test running at the same time
//! can be handed the same port for its own listener, and a request meant to
//! be refused then reaches that test's server instead. Here the port is held
//! by the local end of an established loopback connection: no listener is on
//! it, so a new connection to it is refused (RST), while the operating
//! system does not hand the port out to a listener that asks for port 0.
//! (A socket that is only bound is not
//! enough: macOS then drops the SYN and the connect times out instead of
//! being refused.) Both ends are held until the process exits — nextest
//! runs every test in its own process, so that is the end of the test.
#![allow(dead_code)]

use socket2::{Domain, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Mutex;

static HELD: Mutex<Vec<(Socket, TcpStream)>> = Mutex::new(Vec::new());

/// A port on `ip` with no listener that stays taken; see the module docs.
pub fn refused_port_on(ip: IpAddr) -> std::io::Result<u16> {
    let address = SocketAddr::new(ip, 0);
    let helper = std::net::TcpListener::bind(address)?;
    let socket = Socket::new(Domain::for_address(address), Type::STREAM, None)?;
    if ip.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    // No SO_REUSEADDR: another socket must not be able to share the port.
    socket.bind(&address.into())?;
    socket.connect(&helper.local_addr()?.into())?;
    let (peer, _) = helper.accept()?;
    let port = socket
        .local_addr()?
        .as_socket()
        .map(|address| address.port())
        .ok_or_else(|| std::io::Error::other("bound socket has no inet address"))?;
    HELD.lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((socket, peer));
    Ok(port)
}

/// A `127.0.0.1` port that refuses connections; see the module docs.
pub fn refused_port() -> u16 {
    refused_port_on(IpAddr::V4(Ipv4Addr::LOCALHOST)).expect("bind a refused loopback port")
}

#[test]
fn a_held_port_refuses_connections_and_is_not_handed_out_again() {
    let port = refused_port();
    let error = std::net::TcpStream::connect(("127.0.0.1", port))
        .expect_err("nothing listens on the held port");
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
    // Listeners on port 0 — what the other tests' mock servers ask for —
    // are not given the held port.
    let others: Vec<_> = (0..64)
        .map(|_| std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap())
        .collect();
    assert!(
        others
            .iter()
            .all(|listener| listener.local_addr().unwrap().port() != port)
    );
    let error = std::net::TcpStream::connect(("127.0.0.1", port)).expect_err("still refused");
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
}
