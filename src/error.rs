use std::io;
use std::net;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    AddrParse(net::AddrParseError),
}

impl ::std::error::Error for Error {
    fn cause(&self) -> Option<&::std::error::Error> {
        match *self {
            Error::Io(ref err) => Some(err),
            Error::AddrParse(ref err) => Some(err),
        }
    }

    fn description(&self) -> &str {
        match *self {
            Error::Io(ref err) => err.description(),
            Error::AddrParse(ref err) => err.description(),
        }
    }
}

impl ::std::fmt::Display for Error {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        match *self {
            Error::Io(ref err) => write!(f, "Linebased Server Error: {}", err),
            Error::AddrParse(ref err) => write!(f, "Linebased Server Error: {}", err),
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

pub type Result<T> = ::std::result::Result<T, Error>;
