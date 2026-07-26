use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::NamedTempFile;

use super::error::SigningError;

/// Signs `payload` with the OpenSSH detached-signature format via
/// `ssh-keygen -Y sign`, in the given signature `domain` (OpenSSH
/// namespace). For offline operator/tooling use, not part of the runtime
/// verification trust boundary.
pub fn sign(private_key_path: &Path, domain: &str, payload: &[u8]) -> Result<Vec<u8>, SigningError> {
    let mut child = Command::new("ssh-keygen")
        .arg("-Y")
        .arg("sign")
        .arg("-f")
        .arg(private_key_path)
        .arg("-n")
        .arg(domain)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(SigningError::SshKeygenSpawn)?;

    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(payload)?;

    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(SigningError::SshSignFailed {
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Verifies a detached OpenSSH signature via `ssh-keygen -Y verify`.
///
/// Signature validity, principal authorization (via `allowed_signers_path`),
/// namespace/domain scoping, and revocation (via `revoked_keys_path`) are all
/// enforced by OpenSSH itself.
///
/// Deliberately `pub(super)`, not `pub`: taking the trust-root paths as
/// arguments is exactly the path-injection capability the `signing-test-util`
/// feature gate exists to withhold from downstream code, and this function
/// performs *only* the OpenSSH-level checks — it does not run the
/// domain/schema/scope/no-numbers/canonical-form checks that
/// [`super::verify`] layers on top. Callers outside `signing` must go through
/// [`super::verify`] (or, in test builds, `super::verify_with_paths`).
pub(super) fn verify_detached(
    allowed_signers_path: &Path,
    revoked_keys_path: &Path,
    principal: &str,
    domain: &str,
    payload: &[u8],
    signature: &[u8],
) -> Result<(), SigningError> {
    let mut sig_file = NamedTempFile::new()?;
    sig_file.write_all(signature)?;
    sig_file.flush()?;

    // -vvv is required: ssh-keygen only logs "Key is revoked" at debug3
    // verbosity (verified experimentally against OpenSSH_10.2p1) -- without
    // it, a revoked-key failure is indistinguishable from any other
    // verification failure on stderr.
    let mut child = Command::new("ssh-keygen")
        .arg("-vvv")
        .arg("-Y")
        .arg("verify")
        .arg("-f")
        .arg(allowed_signers_path)
        .arg("-I")
        .arg(principal)
        .arg("-n")
        .arg(domain)
        .arg("-r")
        .arg(revoked_keys_path)
        .arg("-s")
        .arg(sig_file.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(SigningError::SshKeygenSpawn)?;

    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(payload)?;

    let output = child.wait_with_output()?;
    if output.status.success() {
        return Ok(());
    }

    // Classification below is ADVISORY ONLY and version-sensitive: it greps
    // for an OpenSSH debug3 log string ("Key is revoked") whose wording is
    // not part of any stable interface and may change or disappear between
    // OpenSSH releases. It is never a security boundary — both branches fail
    // closed with an error, and the *decision* to reject a revoked key is
    // made by ssh-keygen's non-zero exit status, not by this string match.
    // Do not build policy on top of distinguishing these two variants.
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if stderr.to_lowercase().contains("revoked") {
        Err(SigningError::RevokedKey { stderr })
    } else {
        Err(SigningError::SshVerifyFailed { stderr })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn generate_ed25519_keypair(dir: &Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let key_path = dir.join(name);
        let status = Command::new("ssh-keygen")
            .arg("-t")
            .arg("ed25519")
            .arg("-f")
            .arg(&key_path)
            .arg("-N")
            .arg("")
            .arg("-C")
            .arg("test")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("failed to spawn ssh-keygen -t ed25519");
        assert!(status.success());
        let pub_key_path = key_path.with_extension("pub");
        (key_path, pub_key_path)
    }

    #[test]
    fn sign_then_verify_round_trip_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let (key_path, pub_key_path) = generate_ed25519_keypair(dir.path(), "id_ed25519");
        let pub_key = fs::read_to_string(&pub_key_path).unwrap();

        let allowed_signers_path = dir.path().join("allowed_signers");
        fs::write(
            &allowed_signers_path,
            format!("tester namespaces=\"test.domain\" {pub_key}"),
        )
        .unwrap();

        let revoked_keys_path = dir.path().join("revoked_keys");
        fs::write(&revoked_keys_path, "").unwrap();

        let payload = b"hello world";
        let signature = sign(&key_path, "test.domain", payload).unwrap();

        verify_detached(
            &allowed_signers_path,
            &revoked_keys_path,
            "tester",
            "test.domain",
            payload,
            &signature,
        )
        .unwrap();
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        let dir = tempfile::tempdir().unwrap();
        let (key_path, pub_key_path) = generate_ed25519_keypair(dir.path(), "id_ed25519");
        let pub_key = fs::read_to_string(&pub_key_path).unwrap();

        let allowed_signers_path = dir.path().join("allowed_signers");
        fs::write(
            &allowed_signers_path,
            format!("tester namespaces=\"test.domain\" {pub_key}"),
        )
        .unwrap();
        let revoked_keys_path = dir.path().join("revoked_keys");
        fs::write(&revoked_keys_path, "").unwrap();

        let payload = b"hello world";
        let signature = sign(&key_path, "test.domain", payload).unwrap();

        let err = verify_detached(
            &allowed_signers_path,
            &revoked_keys_path,
            "tester",
            "test.domain",
            b"tampered payload",
            &signature,
        )
        .unwrap_err();
        assert!(matches!(err, SigningError::SshVerifyFailed { .. }));
    }

    #[test]
    fn verify_rejects_revoked_key() {
        let dir = tempfile::tempdir().unwrap();
        let (key_path, pub_key_path) = generate_ed25519_keypair(dir.path(), "id_ed25519");
        let pub_key = fs::read_to_string(&pub_key_path).unwrap();

        let allowed_signers_path = dir.path().join("allowed_signers");
        fs::write(
            &allowed_signers_path,
            format!("tester namespaces=\"test.domain\" {pub_key}"),
        )
        .unwrap();
        let revoked_keys_path = dir.path().join("revoked_keys");
        fs::write(&revoked_keys_path, &pub_key).unwrap();

        let payload = b"hello world";
        let signature = sign(&key_path, "test.domain", payload).unwrap();

        let err = verify_detached(
            &allowed_signers_path,
            &revoked_keys_path,
            "tester",
            "test.domain",
            payload,
            &signature,
        )
        .unwrap_err();
        match err {
            SigningError::RevokedKey { stderr } => {
                assert!(stderr.to_lowercase().contains("revoked"))
            }
            other => panic!("expected RevokedKey, got {other:?}"),
        }
    }
}
