//! Error type shared by all transport backends.

use std::io;
use thiserror::Error;

#[derive(Error, Debug)]
/// Possible errors
pub enum Error {
    /// CAN device could not be found
    #[cfg(target_os = "linux")]
    #[error("Failed to find can device: {source:?}")]
    Lookup {
        #[from]
        source: nix::Error,
    },

    /// IO Error
    #[error("IO error: {source:?}")]
    Io {
        #[from]
        source: io::Error,
    },

    /// The interface address could not be used: unknown transport scheme,
    /// or the scheme's backend is not compiled in / not available on this
    /// platform.
    #[error("address error: {0}")]
    Address(String),

    /// The requested socket options are not supported by the selected
    /// backend (e.g. extended addressing on the userspace backends).
    #[error("unsupported option: {0}")]
    Unsupported(String),
}
