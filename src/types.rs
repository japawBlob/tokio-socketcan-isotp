//! Platform-independent ISO-TP option types, behaviour flags and CAN
//! constants shared by all transport backends.

use bitflags::bitflags;
pub use embedded_can::{ExtendedId, Id, StandardId};
use std::convert::TryFrom;
use std::num::TryFromIntError;
use std::os::raw::{c_int, c_short};
use std::time::Duration;

/// CAN address family
pub const AF_CAN: c_short = 29;

/// CAN protocol family
pub const PF_CAN: c_int = 29;

/// ISO 15765-2 Transport Protocol
pub const CAN_ISOTP: c_int = 6;

/// undocumented can.h constant
pub const SOL_CAN_BASE: c_int = 100;

/// undocumented isotp.h constant
pub const SOL_CAN_ISOTP: c_int = SOL_CAN_BASE + CAN_ISOTP;

/// pass struct `IsoTpOptions`
pub const CAN_ISOTP_OPTS: c_int = 1;

/// pass struct `FlowControlOptions`
pub const CAN_ISOTP_RECV_FC: c_int = 2;

/// pass __u32 value in nano secs
/// use this time instead of value
/// provided in FC from the receiver
pub const CAN_ISOTP_TX_STMIN: c_int = 3;

/// pass __u32 value in nano secs
/// ignore received CF frames which
/// timestamps differ less than val
pub const CAN_ISOTP_RX_STMIN: c_int = 4;

/// pass struct `LinkLayerOptions`
pub const CAN_ISOTP_LL_OPTS: c_int = 5;

/// `CAN_MAX_DLEN` According to ISO 11898-1
pub const CAN_MAX_DLEN: u8 = 8;

/// Size of buffer allocated for reading TP data
pub const RECV_BUFFER_SIZE: usize = 4096;

/// Size of a canframe, constant to reduce crate dependencies
/// `std::mem::size_of::<socketcan::CANFrame>())`
pub(crate) const SIZE_OF_CAN_FRAME: u8 = 16;

pub(crate) const CAN_ISOTP_DEFAULT_RECV_BS: u8 = 0;

pub(crate) const CAN_ISOTP_DEFAULT_RECV_STMIN: u8 = 0x00;

pub(crate) const CAN_ISOTP_DEFAULT_RECV_WFTMAX: u8 = 0;

bitflags! {
    pub struct IsoTpBehaviour: u32 {
        /// listen only (do not send FC)
        const CAN_ISOTP_LISTEN_MODE = 0x001;
        /// enable extended addressing
        const CAN_ISOTP_EXTEND_ADDR	= 0x002;
        /// enable CAN frame padding tx path
        const CAN_ISOTP_TX_PADDING	= 0x004;
        /// enable CAN frame padding rx path
        const CAN_ISOTP_RX_PADDING	= 0x008;
        /// check received CAN frame padding
        const CAN_ISOTP_CHK_PAD_LEN	= 0x010;
        /// check received CAN frame padding
        const CAN_ISOTP_CHK_PAD_DATA = 0x020;
        /// half duplex error state handling
        const CAN_ISOTP_HALF_DUPLEX = 0x040;
        /// ignore stmin from received FC
        const CAN_ISOTP_FORCE_TXSTMIN = 0x080;
        /// ignore CFs depending on rx stmin
        const CAN_ISOTP_FORCE_RXSTMIN = 0x100;
        /// different rx extended addressing
        const CAN_ISOTP_RX_EXT_ADDR = 0x200;
    }
}

/// if set, indicate 29 bit extended format
pub const EFF_FLAG: u32 = 0x8000_0000;

/// remote transmission request flag
pub const RTR_FLAG: u32 = 0x4000_0000;

/// error flag
pub const ERR_FLAG: u32 = 0x2000_0000;

/// valid bits in standard frame id
pub const SFF_MASK: u32 = 0x0000_07ff;

/// valid bits in extended frame id
pub const EFF_MASK: u32 = 0x1fff_ffff;

/// valid bits in error frame
pub const ERR_MASK: u32 = 0x1fff_ffff;

/// an error mask that will cause Socketcan to report all errors
pub const ERR_MASK_ALL: u32 = ERR_MASK;

/// an error mask that will cause Socketcan to silently drop all errors
pub const ERR_MASK_NONE: u32 = 0;

/// Convert an [Id] to its raw `can_id` representation including the
/// [EFF_FLAG] bit for extended ids.
pub(crate) fn id_to_raw(id: Id) -> u32 {
    match id {
        Id::Standard(standard_id) => u32::from(standard_id.as_raw()),
        Id::Extended(extended_id) => extended_id.as_raw() | EFF_FLAG,
    }
}

/// ISO-TP otions aka `can_isotp_options`
#[repr(C)]
pub struct IsoTpOptions {
    /// set flags for isotp behaviour.
    flags: u32,
    /// frame transmission time (N_As/N_Ar)
    /// time in nano secs
    frame_txtime: u32,
    /// set address for extended addressing
    ext_address: u8,
    /// set content of padding byte (tx)
    txpad_content: u8,
    /// set content of padding byte (rx)
    rxpad_content: u8,
    /// set address for extended addressing
    rx_ext_address: u8,
}

impl IsoTpOptions {
    pub fn new(
        flags: IsoTpBehaviour,
        frame_txtime: Duration,
        ext_address: u8,
        txpad_content: u8,
        rxpad_content: u8,
        rx_ext_address: u8,
    ) -> Result<Self, TryFromIntError> {
        let flags = flags.bits();
        let frame_txtime = u32::try_from(frame_txtime.as_nanos())?;

        Ok(Self {
            flags,
            frame_txtime,
            ext_address,
            txpad_content,
            rxpad_content,
            rx_ext_address,
        })
    }

    /// get flags for isotp behaviour.
    pub fn get_flags(&self) -> Option<IsoTpBehaviour> {
        IsoTpBehaviour::from_bits(self.flags)
    }

    /// set flags for isotp behaviour.
    pub fn set_flags(&mut self, flags: IsoTpBehaviour) {
        self.flags = flags.bits();
    }

    /// get frame transmission time (N_As/N_Ar)
    pub fn get_frame_txtime(&self) -> Duration {
        Duration::from_nanos(self.frame_txtime.into())
    }

    /// set frame transmission time (N_As/N_Ar)
    pub fn set_frame_txtime(&mut self, frame_txtime: Duration) -> Result<(), TryFromIntError> {
        self.frame_txtime = u32::try_from(frame_txtime.as_nanos())?;
        Ok(())
    }

    /// get frame transmission time (N_As/N_Ar)
    pub fn get_ext_address(&self) -> u8 {
        self.ext_address
    }

    /// set address for extended addressing
    pub fn set_ext_address(&mut self, ext_address: u8) {
        self.ext_address = ext_address;
    }

    /// get address for extended addressing
    pub fn get_txpad_content(&self) -> u8 {
        self.txpad_content
    }

    /// set content of padding byte (tx)
    pub fn set_txpad_content(&mut self, txpad_content: u8) {
        self.txpad_content = txpad_content;
    }

    /// get content of padding byte (rx)
    pub fn get_rxpad_content(&self) -> u8 {
        self.rxpad_content
    }

    /// set content of padding byte (rx)
    pub fn set_rxpad_content(&mut self, rxpad_content: u8) {
        self.rxpad_content = rxpad_content;
    }

    /// get address for extended addressing
    pub fn get_rx_ext_address(&self) -> u8 {
        self.rx_ext_address
    }

    /// set address for extended addressing
    pub fn set_rx_ext_address(&mut self, rx_ext_address: u8) {
        self.rx_ext_address = rx_ext_address;
    }
}

impl Default for IsoTpOptions {
    fn default() -> Self {
        // Defaults defined in linux/can/isotp.h
        Self {
            flags: 0x00,
            frame_txtime: 0x00,
            ext_address: 0x00,
            txpad_content: 0xCC,
            rxpad_content: 0xCC,
            rx_ext_address: 0x00,
        }
    }
}

/// Flow control options aka `can_isotp_fc_options`
#[repr(C)]
pub struct FlowControlOptions {
    /// blocksize provided in FC frame
    /// 0 = off
    bs: u8,
    /// separation time provided in FC frame
    ///
    /// 0x00 - 0x7F : 0 - 127 ms
    /// 0x80 - 0xF0 : reserved
    /// 0xF1 - 0xF9 : 100 us - 900 us
    /// 0xFA - 0xFF : reserved
    stmin: u8,
    /// max. number of wait frame transmiss.
    /// 0 = omit FC N_PDU WT
    wftmax: u8,
}

impl Default for FlowControlOptions {
    fn default() -> Self {
        Self {
            bs: CAN_ISOTP_DEFAULT_RECV_BS,
            stmin: CAN_ISOTP_DEFAULT_RECV_STMIN,
            wftmax: CAN_ISOTP_DEFAULT_RECV_WFTMAX,
        }
    }
}

impl FlowControlOptions {
    /// Creates new flow control options
    /// # Parameters
    /// * bs - Blocksize for the ISO-TP frame, set 0 for off (Sending ECU shall send all frames in 1 block)
    /// * stmin - Separation time between consecutive frames. The following values are allowed:
    ///     * 0x00 - 0x7F : 0 - 127 ms
    ///     * 0xF1 - 0xF9 : 100 - 900 us
    /// * wftmax - Maximum number of wait frame transmiss
    ///     * Default value is 0 (omit)
    pub fn new(bs: u8, stmin: u8, wftmax: u8) -> Self {
        Self { bs, stmin, wftmax }
    }

    /// get blocksize provided in FC frame (0 = off)
    pub fn get_bs(&self) -> u8 {
        self.bs
    }

    /// get separation time provided in FC frame
    pub fn get_stmin(&self) -> u8 {
        self.stmin
    }

    /// get max. number of wait frame transmissions (0 = omit FC N_PDU WT)
    pub fn get_wftmax(&self) -> u8 {
        self.wftmax
    }
}

bitflags! {
    pub struct TxFlags: u8 {
        /// bit rate switch (second bitrate for payload data)
        const CANFD_BRS = 0x01;
        /// error state indicator of the transmitting node
        const CANFD_ESI	= 0x02;
    }
}

/// Link layer options aka `can_isotp_ll_options`
#[repr(C)]
pub struct LinkLayerOptions {
    /// generated & accepted CAN frame type
    /// CAN_MTU   (16) -> standard CAN 2.0
    /// CANFD_MTU (72) -> CAN FD frame
    mtu: u8,
    /// tx link layer data length in bytes
    /// (configured maximum payload length)
    /// __u8 value : 8,12,16,20,24,32,48,64
    /// => rx path supports all LL_DL values
    tx_dl: u8,
    /// set into struct canfd_frame.flags
    /// at frame creation: e.g. CANFD_BRS
    /// Obsolete when the BRS flag is fixed
    /// by the CAN netdriver configuration
    tx_flags: u8,
}

impl LinkLayerOptions {
    pub fn new(mtu: u8, tx_dl: u8, tx_flags: TxFlags) -> Self {
        let tx_flags = tx_flags.bits();
        Self {
            mtu,
            tx_dl,
            tx_flags,
        }
    }
}

impl Default for LinkLayerOptions {
    fn default() -> Self {
        Self {
            // CAN_ISOTP_DEFAULT_LL_MTU
            mtu: SIZE_OF_CAN_FRAME,
            // CAN_ISOTP_DEFAULT_LL_TX_DL
            tx_dl: CAN_MAX_DLEN,
            // CAN_ISOTP_DEFAULT_LL_TX_FLAGS
            tx_flags: 0x00,
        }
    }
}
