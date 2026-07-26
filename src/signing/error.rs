use thiserror::Error;

#[derive(Error, Debug)]
pub enum SigningError {
    #[error("payload contains a JSON number at {path}; numeric values must be decimal strings")]
    NumericValueNotAllowed { path: String },
    #[error("payload domain {found:?} does not match expected domain {expected:?}")]
    DomainMismatch { expected: String, found: String },
    #[error("payload schema_version {found:?} is not one of the accepted versions {accepted:?}")]
    SchemaVersionNotAccepted {
        accepted: Vec<String>,
        found: String,
    },
    #[error("scope must be a JSON object")]
    ScopeNotObject,
    #[error("payload envelope must not contain an embedded \"signature\" field")]
    SignatureFieldNotAllowed,
    #[error("payload scope does not match expected scope")]
    ScopeMismatch,
    #[error("signed payload bytes are not in canonical JCS form")]
    CanonicalFormMismatch,
    #[error("failed to spawn ssh-keygen")]
    SshKeygenSpawn(#[source] std::io::Error),
    #[error("ssh-keygen signing failed: {stderr}")]
    SshSignFailed { stderr: String },
    #[error("ssh-keygen verification failed: {stderr}")]
    SshVerifyFailed { stderr: String },
    #[error("signing key is revoked: {stderr}")]
    RevokedKey { stderr: String },
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
