//! Interop test: userspace ISO-TP (`rawcan:`) against the kernel ISO-TP
//! backend on the same vcan interface. Skips (with a message) when vcan0
//! or the can_isotp kernel module is unavailable.

#![cfg(all(target_os = "linux", feature = "isotp-userspace"))]

use std::time::Duration;
use tokio::time::timeout;
use tokio_socketcan_isotp::{IsoTpBehaviour, IsoTpOptions, IsoTpSocket, StandardId};

const USER_RX: u16 = 0x611;
const USER_TX: u16 = 0x610;

fn padding_opts() -> Option<IsoTpOptions> {
    Some(
        IsoTpOptions::new(
            IsoTpBehaviour::CAN_ISOTP_TX_PADDING,
            Duration::ZERO,
            0,
            0x00,
            0x00,
            0,
        )
        .unwrap(),
    )
}

fn open_kernel_side() -> Option<IsoTpSocket> {
    if !std::path::Path::new("/sys/class/net/vcan0").exists() {
        eprintln!("skipping: vcan0 not available");
        return None;
    }
    // Kernel side mirrors the user side: receives what the user sends.
    match IsoTpSocket::open_with_opts(
        "vcan0",
        StandardId::new(USER_TX).unwrap(),
        StandardId::new(USER_RX).unwrap(),
        padding_opts(),
        None,
        None,
    ) {
        Ok(sock) => Some(sock),
        Err(err) => {
            eprintln!("skipping: kernel ISO-TP unavailable: {err}");
            None
        }
    }
}

async fn exchange(tx: &IsoTpSocket, rx: &IsoTpSocket, payload: &[u8]) {
    tx.write_packet(payload).await.unwrap();
    let received = timeout(Duration::from_secs(5), rx.read_packet())
        .await
        .expect("timed out waiting for ISO-TP payload")
        .unwrap();
    assert_eq!(received, payload);
}

#[tokio::test]
async fn userspace_interops_with_kernel_isotp() {
    let Some(kernel) = open_kernel_side() else {
        return;
    };
    let user = IsoTpSocket::open_with_opts(
        "rawcan:vcan0",
        StandardId::new(USER_RX).unwrap(),
        StandardId::new(USER_TX).unwrap(),
        padding_opts(),
        None,
        None,
    )
    .unwrap();

    let single: Vec<u8> = vec![0x3E, 0x00];
    let multi: Vec<u8> = (0..4095u32).map(|i| (i % 251) as u8).collect();

    // Userspace -> kernel: SF, then segmented (kernel emits real FC).
    exchange(&user, &kernel, &single).await;
    exchange(&user, &kernel, &multi).await;

    // Kernel -> userspace: SF, then segmented (we emit the FC).
    exchange(&kernel, &user, &single).await;
    exchange(&kernel, &user, &multi).await;
}
