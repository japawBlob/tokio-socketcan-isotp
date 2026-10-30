//! Socket-facing wrapper around the userspace ISO-TP state machine.

// identity_op: `PCI_SF | len` and `PCI_FC | FC_CTS` spell out the protocol
// encoding even though both constants are zero.
#[allow(clippy::identity_op)]
mod machine;

use crate::channel::{CAN_MAX_DLEN, FrameChannel, is_valid_fd_len};
use crate::error::Error;
use crate::types::{
    FlowControlOptions, Id, IsoTpBehaviour, IsoTpOptions, LinkLayerOptions, SIZE_OF_CAN_FRAME,
    TxFlags, id_to_raw,
};
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
        link_layer_options: Option<LinkLayerOptions>,
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

        let (tx_dl, fd, brs) = link_layer_config(link_layer_options.as_ref())?;

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
                    tx_dl,
                    fd,
                    brs,
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

/// Validate link layer options like the kernel does (`isotp_setsockopt`
/// `CAN_ISOTP_LL_OPTS`) and return `(tx_dl, fd, brs)`.
fn link_layer_config(opts: Option<&LinkLayerOptions>) -> Result<(usize, bool, bool), Error> {
    let Some(opts) = opts else {
        return Ok((CAN_MAX_DLEN, false, false));
    };
    let tx_dl = usize::from(opts.get_tx_dl());
    if opts.is_can_fd() {
        if !is_valid_fd_len(tx_dl) || tx_dl < CAN_MAX_DLEN {
            return Err(Error::Unsupported(format!(
                "invalid CAN FD tx_dl {tx_dl}: expected 8, 12, 16, 20, 24, 32, 48 or 64"
            )));
        }
        let brs = opts.get_tx_flags().contains(TxFlags::CANFD_BRS);
        Ok((tx_dl, true, brs))
    } else if opts.get_mtu() == SIZE_OF_CAN_FRAME {
        if tx_dl != CAN_MAX_DLEN {
            return Err(Error::Unsupported(format!(
                "tx_dl {tx_dl} requires CAN FD (mtu {})",
                crate::types::CANFD_MTU
            )));
        }
        Ok((CAN_MAX_DLEN, false, false))
    } else {
        Err(Error::Unsupported(format!(
            "invalid link layer mtu {}: expected {SIZE_OF_CAN_FRAME} (CAN) or {} (CAN FD)",
            opts.get_mtu(),
            crate::types::CANFD_MTU
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_layer_defaults_to_classic() {
        assert_eq!(link_layer_config(None).unwrap(), (8, false, false));
        let classic = LinkLayerOptions::default();
        assert_eq!(
            link_layer_config(Some(&classic)).unwrap(),
            (8, false, false)
        );
    }

    #[test]
    fn link_layer_can_fd() {
        let opts = LinkLayerOptions::can_fd(64, TxFlags::CANFD_BRS);
        assert_eq!(link_layer_config(Some(&opts)).unwrap(), (64, true, true));
        let opts = LinkLayerOptions::can_fd(8, TxFlags::empty());
        assert_eq!(link_layer_config(Some(&opts)).unwrap(), (8, true, false));
    }

    #[test]
    fn link_layer_rejects_invalid() {
        assert!(link_layer_config(Some(&LinkLayerOptions::can_fd(10, TxFlags::empty()))).is_err());
        assert!(link_layer_config(Some(&LinkLayerOptions::can_fd(4, TxFlags::empty()))).is_err());
        assert!(link_layer_config(Some(&LinkLayerOptions::new(16, 64, TxFlags::empty()))).is_err());
        assert!(link_layer_config(Some(&LinkLayerOptions::new(42, 8, TxFlags::empty()))).is_err());
    }
}
