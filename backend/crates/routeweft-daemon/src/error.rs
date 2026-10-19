use crate::south::port::routing::RoutingError;
use routeweft_domain::error::DomainError;
use std::fmt;

#[derive(Debug)]
pub enum Error {
    InvalidRequest(String),
    Domain(DomainError),
    Routing(RoutingError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(f, "请求参数错误：{message}"),
            Self::Domain(error) => error.fmt(f),
            Self::Routing(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidRequest(_) => None,
            Self::Domain(error) => Some(error),
            Self::Routing(error) => Some(error),
        }
    }
}

impl From<DomainError> for Error {
    fn from(error: DomainError) -> Self {
        Self::Domain(error)
    }
}

impl From<RoutingError> for Error {
    fn from(error: RoutingError) -> Self {
        Self::Routing(error)
    }
}
