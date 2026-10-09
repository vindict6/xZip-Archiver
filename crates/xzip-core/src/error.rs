#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid settings: {0}")]
    Settings(String),

    /// The data is not a valid .xz stream, or it is damaged.
    #[error("corrupt data: {0}")]
    Corrupt(String),

    /// An archive failed verification. Nothing from it was used.
    #[error("integrity check failed: {0}")]
    Integrity(String),

    /// A path in the archive could not be extracted safely.
    #[error("unsafe path: {0}")]
    UnsafePath(String),

    #[error("{0}")]
    Archive(String),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("cancelled")]
    Cancelled,

    #[error("this archive is password protected")]
    PasswordRequired,

    #[error("wrong password")]
    WrongPassword,
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io_err(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Error {
    let context = context.into();
    move |source| Error::Io { context, source }
}

pub(crate) fn io_path(what: &str, path: &std::path::Path) -> impl FnOnce(std::io::Error) -> Error {
    io_err(format!("{what} {}", path.display()))
}

impl Error {
    pub fn is_integrity(&self) -> bool {
        matches!(
            self,
            Error::Integrity(_) | Error::Corrupt(_) | Error::UnsafePath(_)
        )
    }

    pub fn path_context(self, path: &std::path::Path) -> Error {
        match self {
            Error::Io { context, source } => Error::Io {
                context: format!("{context} ({})", path.display()),
                source,
            },
            other => other,
        }
    }
}
