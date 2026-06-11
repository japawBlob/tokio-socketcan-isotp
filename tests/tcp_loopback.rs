//! Portable loopback test for the TCP transport: a `tcplisten:` frame hub
//! (server role, like the ECU simulator) talking to a `tcp:` client (like
//! the diagnostic tester). Also verifies that the hub and the client
//! connection survive dropping and reopening the server-side socket, which
//! is how the simulator uses sockets (one per transaction).

#![cfg(feature = "tcp")]

use std::time::Duration;
use tokio::time::timeout;
use tokio_socketcan_isotp::{IsoTpBehaviour, IsoTpOptions, IsoTpSocket, StandardId};

const CLIENT_TX: u16 = 0x7E0;
const CLIENT_RX: u16 = 0x7E8;

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

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn open_server(port: u16) -> IsoTpSocket {
    IsoTpSocket::open_with_opts(
        &format!("tcplisten:127.0.0.1:{port}"),
        StandardId::new(CLIENT_TX).unwrap(),
        StandardId::new(CLIENT_RX).unwrap(),
        padding_opts(),
        None,
        None,
    )
    .unwrap()
}

async fn request_response(client: &IsoTpSocket, server: &IsoTpSocket) {
    let request: Vec<u8> = (0..300u32).map(|i| (i % 256) as u8).collect();
    let response = vec![0x62u8; 4095];

    let expected_request = request.clone();
    let expected_response = response.clone();

    let server_side = async {
        let received = server.read_packet().await.unwrap();
        assert_eq!(received, expected_request);
        server.write_packet(&response).await.unwrap();
    };
    let client_side = async {
        client.write_packet(&request).await.unwrap();
        let received = client.read_packet().await.unwrap();
        assert_eq!(received, expected_response);
    };
    timeout(Duration::from_secs(10), async {
        tokio::join!(server_side, client_side)
    })
    .await
    .expect("request/response timed out");
}

#[tokio::test]
async fn hub_and_client_exchange_and_survive_socket_reopen() {
    let port = free_port();

    let server = open_server(port);
    let client = IsoTpSocket::open_with_opts(
        &format!("tcp:127.0.0.1:{port}"),
        StandardId::new(CLIENT_RX).unwrap(),
        StandardId::new(CLIENT_TX).unwrap(),
        padding_opts(),
        None,
        None,
    )
    .unwrap();

    request_response(&client, &server).await;

    // The simulator opens a fresh socket per transaction: the hub (and the
    // client's TCP connection) must survive dropping the server socket.
    drop(server);
    let server = open_server(port);
    request_response(&client, &server).await;
}
