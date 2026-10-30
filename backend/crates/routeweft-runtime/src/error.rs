//! Errors returned by runtime admission, loading and queries.
use routeweft_routing::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeError {
    Overloaded,
    DeadlineExceeded,
    InvalidDataset,
    Internal,
    Engine(Error),
}
impl From<Error> for RuntimeError {
    fn from(value: Error) -> Self {
        Self::Engine(value)
    }
}
impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for RuntimeError {}
