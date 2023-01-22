# tokio-socketcan-isotp

This library sets up to create asynchronous wrapper around [socketcan-isotp](https://github.com/marcelbuesing/socketcan-isotp)

This library is currently work in progres. Basic API is working, but your knowledne may vary. Currently not dependent on the original, but rhather on the fork of the socketcan-isotp libary. This should be resolved in the final version.

You may expirience busy waiting, when write encouters io::Error:WouldBlock.

Here is example of basic echoing server:

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
```
