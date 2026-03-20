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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let cases: Vec<(Error, &str)> = vec![
            (Error::Config("bad toml".into()), "config error:"),
            (
                Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "gone")),
                "io error:",
            ),
            (Error::Database("connection lost".into()), "database error:"),
            (Error::Grpc("timeout".into()), "grpc error:"),
            (Error::Ipp("bad request".into()), "ipp error:"),
            (Error::Print("paper jam".into()), "print error:"),
            (Error::Ipc("pipe broken".into()), "ipc error:"),
        ];

        for (error, expected_prefix) in cases {
            let display = error.to_string();
            assert!(
                display.contains(expected_prefix),
                "Expected '{}' to contain '{}'",
                display,
                expected_prefix
            );
        }
    }
}
