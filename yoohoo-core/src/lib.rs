//! Pure, bounded policies. No IO, credentials, compositor dispatch or background tasks.
//! Adapters must authenticate inputs and revalidate identity at effect execution.
pub mod config;
pub mod history;
pub mod identity;
pub mod reducer;
pub mod selection;
pub mod transport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidIdentity,
    InvalidConfig,
    InvalidInput,
    LimitExceeded,
    StaleIdentity,
    StaleRevision,
    SourceUnavailable,
    ClockWentBackwards,
    SequenceGap,
    Duplicate,
    StreamExpired,
    QueueOverflow,
    Exhausted,
    NotOpen,
    Stopped,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Static diagnostics never echo titles, paths, notification text or IDs.
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
