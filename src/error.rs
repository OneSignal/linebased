use std::{error, fmt, io, net};

/// Error from linebased crate
#[derive(Debug)]
pub enum Error {
    /// Encountered I/O error
    Io(io::Error),

    /// Error parsing listen address
    AddrParse(net::AddrParseError),

    /// No bytes were read from the stream
    NoBytesRead,
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn 'static + std::error::Error)> {
        match *self {
            Error::Io(ref err) => Some(err),
            Error::AddrParse(ref err) => Some(err),
            Error::NoBytesRead => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::Io(ref err) => write!(f, "Linebased Server Error: {}", err),
            Error::AddrParse(ref err) => write!(f, "Error parsing address: {}", err),
            Error::NoBytesRead => write!(f, "No bytes read from stream"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(val: io::Error) -> Error {
        Error::Io(val)
    }
}

impl From<net::AddrParseError> for Error {
    fn from(val: net::AddrParseError) -> Error {
        Error::AddrParse(val)
    }
}

/// Results from linebased crate
pub type Result<T> = ::std::result::Result<T, Error>;
