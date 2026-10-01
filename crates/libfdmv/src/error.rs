use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("not an FDMV file")]
    NotFdmv,
    #[error("unsupported FDMV version {major}.{minor}")]
    UnsupportedVersion { major: u16, minor: u16 },
    #[error("CRC mismatch in block at offset {offset}")]
    Crc { offset: u64 },
    #[error("malformed data: {0}")]
    Malformed(String),
    #[error("invalid argument: {0}")]
    Invalid(String),
    #[error("ffmpeg: {0}")]
    Ffmpeg(String),
    #[error("decoder: {0}")]
    Decoder(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn malformed<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Malformed(msg.into()))
}

pub(crate) fn invalid<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Invalid(msg.into()))
}
