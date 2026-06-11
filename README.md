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
| `tcp:host:port`       | userspace ISO-TP over TCP (connect to a hub)   | feature `tcp`                    |
| `tcplisten:addr:port` | userspace ISO-TP over TCP (bind a frame hub)   | feature `tcp`                    |

- `isotp-userspace` implements an ISO 15765-2 subset (SF/FF/CF/FC, block
  size and STmin pacing, N_Bs/N_Cr timeouts, TX padding, 11/29-bit ids,
  payloads up to 4095 bytes) in userspace, so only `CAN_RAW` support is
  needed - vcan works without the `can_isotp` kernel module. Extended
  addressing, listen mode and CAN FD are not supported; RX padding
  checks, `HALF_DUPLEX`, `FORCE_*STMIN`, `wftmax` and all
  `LinkLayerOptions` are ignored.
- `tcp` (implies `isotp-userspace`) replaces the CAN bus with a TCP
  "virtual bus" and works on all platforms, including Windows. One
  process binds a hub with `tcplisten:`; local sockets and remote `tcp:`
  clients attach to it and every frame is broadcast to all other ports
  (no echo to the sender). Each TCP message is exactly 16 bytes: u32 LE
  can_id (bit 31 = 29-bit id flag), u8 data length, 3 reserved zero
  bytes, 8 data bytes - see `src/channel/tcp.rs`.
