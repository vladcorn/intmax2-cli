use crate::external_api::common::error::ServerError;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("Server error: {0}")]
    ServerError(#[from] ServerError),

    #[error("Witness generation error: {0}")]
    WitnessGenerationError(String),

    #[error("Internal error: {0}")]
    InternalError(String),

    #[error("Sync error: {0}")]
    SyncError(String),

    #[error("Decryption error: {0}")]
    DecryptionError(String),
}
