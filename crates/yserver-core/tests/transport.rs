use std::{
    io::{Read, Write},
    net::Shutdown,
    os::{fd::AsRawFd, unix::net::UnixStream},
    time::Duration,
};

use yserver_core::transport::Transport;

fn unix_pair() -> (Transport, UnixStream) {
    Transport::pair().expect("socketpair")
}

#[test]
fn unix_transport_delegates_io_and_raw_fd() {
    let (mut transport, mut peer) = unix_pair();

    transport.write_all(b"request").expect("write request");
    let mut received = [0; 7];
    peer.read_exact(&mut received).expect("read request");
    assert_eq!(&received, b"request");

    peer.write_all(b"reply").expect("write reply");
    let mut reply = [0; 5];
    transport.read_exact(&mut reply).expect("read reply");
    assert_eq!(&reply, b"reply");
    assert!(transport.as_raw_fd() >= 0);
}

#[test]
fn unix_transport_delegates_socket_configuration_clone_and_shutdown() {
    let (transport, mut peer) = unix_pair();

    transport.set_nonblocking(false).expect("set blocking");
    transport
        .set_read_timeout(Some(Duration::from_millis(25)))
        .expect("set read timeout");
    transport
        .set_write_timeout(Some(Duration::from_millis(25)))
        .expect("set write timeout");

    let mut writer = transport.try_clone().expect("clone transport");
    writer.write_all(b"clone").expect("write through clone");
    let mut received = [0; 5];
    peer.read_exact(&mut received).expect("read clone write");
    assert_eq!(&received, b"clone");

    transport.shutdown(Shutdown::Write).expect("shutdown write");
    let mut eof = [0; 1];
    assert_eq!(peer.read(&mut eof).expect("read EOF"), 0);
}
