//! Parsing of the interface address string into a transport selection.
//!
//! The address decides which backend a socket uses:
//!
//! | Address                    | Backend                                      |
//! |----------------------------|----------------------------------------------|
//! | `vcan0`, `can0`, ...       | kernel ISO-TP socket (Linux `can_isotp`)     |
//! | `rawcan:vcan0`             | userspace ISO-TP over a `CAN_RAW` socket     |
//! | `tcp:host:port`            | userspace ISO-TP over TCP (connect)          |
//! | `tcplisten:addr:port`      | userspace ISO-TP over TCP (listen, frame hub)|
//! | `socketcand:host:port:bus` | userspace ISO-TP over a socketcand daemon    |
//!
//! Plain interface names must not contain `:`; anything containing a `:`
//! that does not match a known scheme is rejected so that typos fail loudly
//! instead of being treated as a kernel interface name.

use crate::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Address<'a> {
    /// Plain interface name for the kernel ISO-TP backend.
    Kernel(&'a str),
    /// `rawcan:<ifname>`: userspace ISO-TP over a raw SocketCAN socket.
    RawCan(&'a str),
    /// `tcp:<host>:<port>`: userspace ISO-TP over a TCP client connection.
    TcpConnect(&'a str),
    /// `tcplisten:<addr>:<port>`: userspace ISO-TP attached to a local
    /// TCP frame hub which accepts client connections.
    TcpListen(&'a str),
    /// `socketcand:<host>:<port>:<bus>`: userspace ISO-TP over a socketcand
    /// daemon connection in rawmode.
    Socketcand { endpoint: &'a str, bus: &'a str },
}

pub(crate) fn parse(address: &str) -> Result<Address<'_>, Error> {
    if let Some(ifname) = address.strip_prefix("rawcan:") {
        if ifname.is_empty() {
            return Err(Error::Address(format!(
                "missing interface name in '{address}'"
            )));
        }
        return Ok(Address::RawCan(ifname));
    }
    if let Some(endpoint) = address.strip_prefix("tcplisten:") {
        if endpoint.is_empty() {
            return Err(Error::Address(format!(
                "missing listen address in '{address}'"
            )));
        }
        return Ok(Address::TcpListen(endpoint));
    }
    if let Some(rest) = address.strip_prefix("socketcand:") {
        // `host:port:bus` - the bus name is the final `:`-separated token.
        let (endpoint, bus) = rest
            .rsplit_once(':')
            .filter(|(endpoint, bus)| !endpoint.is_empty() && !bus.is_empty())
            .ok_or_else(|| {
                Error::Address(format!(
                    "expected 'socketcand:<host>:<port>:<bus>' in '{address}'"
                ))
            })?;
        return Ok(Address::Socketcand { endpoint, bus });
    }
    if let Some(endpoint) = address.strip_prefix("tcp:") {
        if endpoint.is_empty() {
            return Err(Error::Address(format!("missing host:port in '{address}'")));
        }
        return Ok(Address::TcpConnect(endpoint));
    }
    if address.contains(':') {
        return Err(Error::Address(format!(
            "unknown transport scheme in '{address}' \
             (expected a plain interface name, 'rawcan:<ifname>', \
             'tcp:<host>:<port>', 'tcplisten:<addr>:<port>' or \
             'socketcand:<host>:<port>:<bus>')"
        )));
    }
    Ok(Address::Kernel(address))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_name_is_kernel() {
        assert_eq!(parse("vcan0").unwrap(), Address::Kernel("vcan0"));
    }

    #[test]
    fn rawcan_scheme() {
        assert_eq!(parse("rawcan:vcan0").unwrap(), Address::RawCan("vcan0"));
    }

    #[test]
    fn tcp_schemes() {
        assert_eq!(
            parse("tcp:127.0.0.1:9800").unwrap(),
            Address::TcpConnect("127.0.0.1:9800")
        );
        assert_eq!(
            parse("tcplisten:0.0.0.0:9800").unwrap(),
            Address::TcpListen("0.0.0.0:9800")
        );
    }

    #[test]
    fn socketcand_scheme() {
        assert_eq!(
            parse("socketcand:127.0.0.1:29536:vcan0").unwrap(),
            Address::Socketcand {
                endpoint: "127.0.0.1:29536",
                bus: "vcan0"
            }
        );
    }

    #[test]
    fn unknown_scheme_is_rejected() {
        assert!(parse("udp:127.0.0.1:9800").is_err());
        assert!(parse("rawcan:").is_err());
        assert!(parse("tcp:").is_err());
        assert!(parse("tcplisten:").is_err());
        assert!(parse("socketcand:").is_err());
        assert!(parse("socketcand:vcan0").is_err());
    }
}
