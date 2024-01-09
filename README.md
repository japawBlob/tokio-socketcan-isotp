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
            
    while let Ok(packet) = socket.read_packet()?.await {
        println!("{:?}", packet);
        let rx = socket.write_packet(packet)?.await;
    }
}
```

To setup vcan0 run following commands:

```bash
sudo modprobe can
sudo ip link add dev vcan0 type vcan
sudo ip link set up vcan0
```
