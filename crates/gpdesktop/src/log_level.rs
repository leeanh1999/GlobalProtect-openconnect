#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
  Error,
  Warn,
  Info,
  Debug,
}

impl LogLevel {
  /// Fixed-width tag used in the connection log.
  pub fn tag(self) -> &'static str {
    match self {
      LogLevel::Error => "ERROR",
      LogLevel::Warn => "WARN ",
      LogLevel::Info => "INFO ",
      LogLevel::Debug => "DEBUG",
    }
  }
}
