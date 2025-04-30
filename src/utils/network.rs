use std::{
    net::{IpAddr, UdpSocket},
    str::FromStr,
};

use log::warn;

pub fn _check_ipv6_available() -> bool {
    let addr = "::1"; // Loopback IPv6 address

    match IpAddr::from_str(addr) {
        Ok(IpAddr::V6(_)) => true,

        _ => false,
    }
}

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
