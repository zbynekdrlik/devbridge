use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Database(String),

    #[error("grpc error: {0}")]
    Grpc(String),

    #[error("ipp error: {0}")]
    Ipp(String),

    #[error("print error: {0}")]
    Print(String),

    #[error("ipc error: {0}")]
    Ipc(String),
}
