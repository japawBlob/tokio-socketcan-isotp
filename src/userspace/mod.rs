//! Socket-facing wrapper around the userspace ISO-TP state machine.

// identity_op: `PCI_SF | len` and `PCI_FC | FC_CTS` spell out the protocol
// encoding even though both constants are zero.
#[allow(clippy::identity_op)]
mod machine;

use crate::channel::FrameChannel;
use crate::error::Error;
use crate::types::{id_to_raw, FlowControlOptions, Id, IsoTpBehaviour, IsoTpOptions};
use std::io;
use tokio::sync::Mutex;

/// A userspace ISO-TP endpoint over a raw CAN frame channel.
///
/// Unlike the kernel backend, a single socket does not support concurrent
/// `read_packet` and `write_packet` calls: the state machine owns the
/// channel exclusively while one operation is in flight. The consumers of
/// this crate use strict request/response patterns, where this never
/// happens.
pub(crate) struct UserspaceIsoTp {
    inner: Mutex<UsInner>,
}

struct UsInner {
    chan: FrameChannel,
    cfg: machine::IsoTpConfig,
}

impl UserspaceIsoTp {
    pub(crate) fn new(
        chan: FrameChannel,
        src: Id,
        dst: Id,
        isotp_options: Option<IsoTpOptions>,
        rx_flow_control_options: Option<FlowControlOptions>,
    ) -> Result<Self, Error> {
        let rx_id = id_to_raw(src);
        let tx_id = id_to_raw(dst);

        let (tx_padding, frame_txtime) = match &isotp_options {
            Some(opts) => {
                let flags = opts.get_flags().ok_or_else(|| {
                    Error::Unsupported("unknown ISO-TP behaviour flags".to_owned())
                })?;
                let unsupported = IsoTpBehaviour::CAN_ISOTP_LISTEN_MODE
                    | IsoTpBehaviour::CAN_ISOTP_EXTEND_ADDR
                    | IsoTpBehaviour::CAN_ISOTP_RX_EXT_ADDR;
                if flags.intersects(unsupported) {
                    return Err(Error::Unsupported(
                        "listen mode and extended addressing are not supported \
                         by the userspace ISO-TP backend"
                            .to_owned(),
                    ));
                }
                let tx_padding = flags
                    .contains(IsoTpBehaviour::CAN_ISOTP_TX_PADDING)
                    .then(|| opts.get_txpad_content());
                (tx_padding, opts.get_frame_txtime())
            }
            None => (None, std::time::Duration::ZERO),
        };

        let (fc_bs, fc_stmin) = match &rx_flow_control_options {
            Some(fc) => (fc.get_bs(), fc.get_stmin()),
            None => (0, 0),
        };

        Ok(UserspaceIsoTp {
            inner: Mutex::new(UsInner {
                chan,
                cfg: machine::IsoTpConfig {
                    tx_id,
                    rx_id,
                    tx_padding,
                    frame_txtime,
                    fc_bs,
                    fc_stmin,
                },
            }),
        })
    }

    pub(crate) async fn write_packet(&self, packet: &[u8]) -> io::Result<()> {
        let mut inner = self.inner.lock().await;
        let UsInner { chan, cfg } = &mut *inner;
        machine::send(chan, cfg, packet).await
    }

    pub(crate) async fn read_packet(&self) -> io::Result<Vec<u8>> {
        let mut inner = self.inner.lock().await;
        let UsInner { chan, cfg } = &mut *inner;
        machine::recv(chan, cfg).await
    }
}
