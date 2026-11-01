use std::net::UdpSocket;

use log::warn;

pub fn check_ipv6_available() -> bool {
    let sender_socket_result = UdpSocket::bind("[::]:0");
    match sender_socket_result {
        Ok(_) => true,
        Err(err) => {
            warn!("Error binding to IPv6 socket: {}", err);
            false
        }
    }
}
