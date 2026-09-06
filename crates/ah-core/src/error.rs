use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Http(String),
    /// Non-2xx from the API, with status and body excerpt.
    Api {
        status: u16,
        message: String,
    },
    Json(serde_json::Error),
    Config(String),
    Plugin {
        plugin: String,
        message: String,
    },
    Auth(String),
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Http(e) => write!(f, "http: {e}"),
            Error::Api { status, message } => write!(f, "api {status}: {message}"),
            Error::Json(e) => write!(f, "json: {e}"),
            Error::Config(e) => write!(f, "config: {e}"),
            Error::Plugin { plugin, message } => write!(f, "plugin {plugin}: {message}"),
            Error::Auth(e) => write!(f, "auth: {e}"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}
impl From<ureq::Error> for Error {
    fn from(e: ureq::Error) -> Self {
        match e {
            ureq::Error::StatusCode(s) => Error::Api {
                status: s,
                message: String::new(),
            },
            ureq::Error::Io(io) => Error::Io(io),
            other => Error::Http(other.to_string()),
        }
    }
}
