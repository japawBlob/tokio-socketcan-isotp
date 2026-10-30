# tokio-socketcan-isotp

This library creates asynchronous wrapper around [socketcan-isotp](https://github.com/marcelbuesing/socketcan-isotp)

Currently not dependent on the original, but rather on the modified of the socketcan-isotp library, which adds read_to_vec method, in order to prevent mutable borrowing of the socket, when reading. 

You may experience busy waiting, or soft locks when write encounters io::Error:WouldBlock. This is due to the error in Linux Kernel, which is now [solved](https://lore.kernel.org/linux-can/20230818114345.142983-1-lukas.magel@posteo.net/), but the maintainer of your Linux kernel might not have shipped it yet. I tested it on the mainline Kernel 6.6.5 and everything worked correctly. If you experience mentioned errors and cannot upgrade to newer kernel, refer to the src/lib.rs file to the poll method of the IsoTpWriteFuture, where a edit is suggested that might help with your situation.

Example of basic echoing server on vcan0:

```rust
use tokio_socketcan_isotp::{IsoTpSocket, StandardId, Error};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut socket = IsoTpSocket::open(
        "vcan0",
        StandardId::new(0x123).expect("Invalid src id"),
        StandardId::new(0x321).expect("Invalid src id")
            )?;
            
    while let Ok(packet) = socket.read_packet().await {
        println!("{:?}", packet);
        let rx = socket.write_packet(packet).await;
    }
}
```

To setup vcan0 run following commands:

```bash
sudo modprobe can
sudo ip link add dev vcan0 type vcan
sudo ip link set up vcan0
```

## Transport backends and feature flags

The backend is selected at runtime by the address string passed to
`IsoTpSocket::open` / `open_with_opts`:

| Address               | Backend                                        | Requires                         |
|-----------------------|------------------------------------------------|----------------------------------|
| `vcan0`, `can0`, ...  | kernel ISO-TP socket (`can_isotp` module)      | Linux                            |
| `rawcan:vcan0`        | userspace ISO-TP over a `CAN_RAW` socket       | Linux, feature `isotp-userspace` |
| `socketcand:host:port:bus` | userspace ISO-TP over a socketcand daemon | feature `socketcand`          |

- `isotp-userspace` implements an ISO 15765-2 subset (SF/FF/CF/FC, block
  size and STmin pacing, N_Bs/N_Cr timeouts, TX padding, 11/29-bit ids) in
  userspace, so only `CAN_RAW` support is needed - vcan works without the
  `can_isotp` kernel module. CAN FD (ISO 15765-2:2016) is supported and
  selected like for the kernel backend via `LinkLayerOptions`
  (`mtu = 72`, `tx_dl` 8..=64, `CANFD_BRS`), e.g.
  `LinkLayerOptions::can_fd(64, TxFlags::CANFD_BRS)`: escaped single frames,
  32-bit first frame lengths (payloads > 4095 bytes, also on classic CAN),
  and padding to valid CAN FD frame lengths. The receive path accepts all
  link layer data lengths. Extended addressing and listen mode are not
  supported; RX padding checks, `HALF_DUPLEX`, `FORCE_*STMIN` and `wftmax`
  are ignored.
- `socketcand` (implies `isotp-userspace`) connects to a
  [socketcand](https://github.com/linux-can/socketcand) daemon in rawmode
  over TCP and works on all platforms. After the `< hi >` / `< open <bus> >`
  / `< rawmode >` handshake, frames are exchanged as the daemon's ASCII
  `< frame ... >` / `< send ... >` messages - see
  `src/channel/socketcand.rs`. Useful when the CAN bus lives on another host
  or in a container the client cannot attach to directly. Upstream
  socketcand is classic CAN only; CAN FD needs an FD-capable server such as
  [cda-can-bridge](https://github.com/floroks/cda-can-bridge), which exchanges
  FD frames as `< fdframe <id> <flags> <ts> <data> >` / `< fdsend <id> <flags>
  <len> <bytes> >` (syntax of
  [linux-can/socketcand#79](https://github.com/linux-can/socketcand/pull/79)).
