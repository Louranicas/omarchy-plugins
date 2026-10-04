//! Deterministic domain policy, not an ACP wire implementation or authorization sandbox.
//! Effects require authenticated, bounded IO adapters and durable writer acknowledgements.
pub mod config;
pub mod launch;
pub mod permission;
pub mod session;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidInput,
    InvalidConfig,
    Unsupported,
    MissingHarness,
    MissingExecutable,
    ReadFailed,
    StaleGeneration,
    StaleOperation,
    StaleRevision,
    WrongPhase,
    Busy,
    NoActiveTurn,
    LimitExceeded,
    Duplicate,
    UnknownOption,
    UnknownRequest,
    PolicyPending,
    PersistenceFailed,
    Expired,
    ClockWentBackwards,
    Exhausted,
    SequenceGap,
    Closed,
    Timeout,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(String);
impl Id {
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err(Error::InvalidInput);
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Id(<redacted>)")
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct Text(String);
impl Text {
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        if value.len() > 65_536 || value.contains('\0') {
            return Err(Error::LimitExceeded);
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub(crate) fn append(&mut self, value: &Text) {
        self.0.push_str(&value.0);
    }
}
impl std::fmt::Debug for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Text(<redacted>)")
    }
}
/// Preserve JSON-RPC number versus string identity. An adapter must not stringify numeric IDs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequestId {
    Number(i64),
    String(Id),
}
