//! API exceptions, serialised exactly like the reference API: `{ "code", "message" }`.

use axum::http::StatusCode;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// The reference API always raises this with the UUID-parsing message.
    #[error("Argument is not a valid UUID string")]
    InvalidSyncId,
    #[error("Sync does not exist")]
    SyncNotFound,
    #[error("Unable to find required data")]
    RequiredDataNotFound,
    #[error("The service is not accepting new syncs")]
    NewSyncsForbidden,
    #[error("Client has exceeded the daily new syncs limit")]
    NewSyncsLimitExceeded,
    #[error("The requested route has not been implemented")]
    NotImplemented,
    #[error("A sync conflict was detected")]
    SyncConflict,
    #[error("The requested API version is not supported")]
    UnsupportedVersion,
    #[error("Sync data limit exceeded")]
    SyncDataLimitExceeded,
    #[error("Too many requests")]
    RequestThrottled,
    #[error("Client not permitted to access this service")]
    OriginNotPermitted,
    #[error("An unspecified error has occurred")]
    Unspecified,
    #[error("The service is currently offline")]
    ServiceNotAvailable,
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::InvalidSyncId | Self::SyncNotFound => StatusCode::UNAUTHORIZED,
            Self::RequiredDataNotFound => StatusCode::BAD_REQUEST,
            Self::NewSyncsForbidden => StatusCode::METHOD_NOT_ALLOWED,
            Self::NewSyncsLimitExceeded => StatusCode::NOT_ACCEPTABLE,
            Self::NotImplemented => StatusCode::NOT_FOUND,
            Self::SyncConflict => StatusCode::CONFLICT,
            Self::UnsupportedVersion => StatusCode::PRECONDITION_FAILED,
            Self::SyncDataLimitExceeded => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RequestThrottled => StatusCode::TOO_MANY_REQUESTS,
            Self::OriginNotPermitted | Self::Unspecified => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ServiceNotAvailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// Exception name used as the `code` field.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidSyncId => "InvalidSyncIdException",
            Self::SyncNotFound => "SyncNotFoundException",
            Self::RequiredDataNotFound => "RequiredDataNotFoundException",
            Self::NewSyncsForbidden => "NewSyncsForbiddenException",
            Self::NewSyncsLimitExceeded => "NewSyncsLimitExceededException",
            Self::NotImplemented => "NotImplementedException",
            Self::SyncConflict => "SyncConflictException",
            Self::UnsupportedVersion => "UnsupportedVersionException",
            Self::SyncDataLimitExceeded => "SyncDataLimitExceededException",
            Self::RequestThrottled => "RequestThrottledException",
            Self::OriginNotPermitted => "OriginNotPermittedException",
            Self::Unspecified => "UnspecifiedException",
            Self::ServiceNotAvailable => "ServiceNotAvailableException",
        }
    }

    pub fn body(&self) -> serde_json::Value {
        serde_json::json!({ "code": self.code(), "message": self.to_string() })
    }
}

/// Storage failures surface to clients as `UnspecifiedException`, like any unexpected error.
impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        tracing::error!(error = %format!("{err:#}"), "unexpected error");
        Self::Unspecified
    }
}
