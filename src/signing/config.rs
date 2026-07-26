use std::path::PathBuf;

/// Production, code-constant path to the committed `allowed_signers` file.
///
/// Only production code should use this. Tests must use temp fixtures via
/// `verify_with_paths` instead.
pub fn allowed_signers_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/signers/allowed_signers")
}

/// Production, code-constant path to the committed `revoked_keys` file.
///
/// Only production code should use this. Tests must use temp fixtures via
/// `verify_with_paths` instead.
pub fn revoked_keys_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/signers/revoked_keys")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_point_at_committed_config_files() {
        assert!(allowed_signers_path().ends_with("config/signers/allowed_signers"));
        assert!(revoked_keys_path().ends_with("config/signers/revoked_keys"));
    }
}
