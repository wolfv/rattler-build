//! GPG commit signature verification for git sources.
//!
//! Verifies that git commits were signed by trusted contributors by fetching
//! their GPG public keys from GitHub (`https://github.com/{username}.gpg`).

use std::io::Cursor;
use std::path::Path;

use pgp::Deserializable;
use pgp::composed::{SignedPublicKey, StandaloneSignature};
use pgp::types::PublicKeyTrait;

/// Errors that can occur during signature verification
#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("commit {0} is not signed")]
    NotSigned(String),

    #[error("failed to extract commit object: {0}")]
    CommitExtraction(String),

    #[error("failed to parse signature: {0}")]
    SignatureParse(String),

    #[error("failed to fetch GPG keys for user '{0}': {1}")]
    KeyFetch(String, String),

    #[error("failed to parse GPG keys: {0}")]
    KeyParse(String),

    #[error("signature verification failed: commit was not signed by any of the expected signers: {0:?}")]
    VerificationFailed(Vec<String>),
}

/// Extract the GPG signature and the signed payload from a raw git commit object.
///
/// Returns `(ascii_armored_signature, signed_payload_bytes)`.
/// The signed payload is the commit object with the `gpgsig` header removed.
pub fn extract_commit_signature(
    repo_path: &Path,
    commit_hash: &str,
) -> Result<(String, Vec<u8>), SigningError> {
    // Run git cat-file to get the raw commit object
    let output = std::process::Command::new("git")
        .current_dir(repo_path)
        .args(["cat-file", "commit", commit_hash])
        .output()
        .map_err(|e| SigningError::CommitExtraction(format!("failed to run git: {}", e)))?;

    if !output.status.success() {
        return Err(SigningError::CommitExtraction(format!(
            "git cat-file failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let commit_content = String::from_utf8(output.stdout)
        .map_err(|e| SigningError::CommitExtraction(format!("invalid UTF-8 in commit: {}", e)))?;

    // Parse out the gpgsig header.
    // The gpgsig header looks like:
    //   gpgsig -----BEGIN PGP SIGNATURE-----\n
    //    <continuation lines starting with space>\n
    //    -----END PGP SIGNATURE-----\n
    let mut signature_lines = Vec::new();
    let mut payload_lines = Vec::new();
    let mut in_signature = false;

    for line in commit_content.lines() {
        if line.starts_with("gpgsig ") {
            in_signature = true;
            // The signature content starts after "gpgsig "
            signature_lines.push(line.strip_prefix("gpgsig ").unwrap().to_string());
        } else if in_signature {
            if line.starts_with(' ') {
                // Continuation line - strip the leading space
                signature_lines.push(line[1..].to_string());
                if line.contains("-----END PGP SIGNATURE-----") {
                    in_signature = false;
                }
            } else {
                // End of signature block
                in_signature = false;
                payload_lines.push(line.to_string());
            }
        } else {
            payload_lines.push(line.to_string());
        }
    }

    if signature_lines.is_empty() {
        return Err(SigningError::NotSigned(commit_hash.to_string()));
    }

    let signature = signature_lines.join("\n");

    // Reconstruct the signed payload: the commit object without the gpgsig header,
    // formatted as git would for signing (header + "\n" + body)
    let payload = payload_lines.join("\n");

    Ok((signature, payload.into_bytes()))
}

/// Fetch GPG public keys for a GitHub user.
pub fn fetch_github_gpg_keys(
    client: &reqwest::blocking::Client,
    username: &str,
) -> Result<Vec<SignedPublicKey>, SigningError> {
    let url = format!("https://github.com/{}.gpg", username);
    let response = client
        .get(&url)
        .send()
        .map_err(|e| SigningError::KeyFetch(username.to_string(), e.to_string()))?;

    if !response.status().is_success() {
        return Err(SigningError::KeyFetch(
            username.to_string(),
            format!("HTTP {}", response.status()),
        ));
    }

    let body = response
        .bytes()
        .map_err(|e| SigningError::KeyFetch(username.to_string(), e.to_string()))?;

    if body.is_empty() {
        return Err(SigningError::KeyFetch(
            username.to_string(),
            "no GPG keys found".to_string(),
        ));
    }

    let cursor = Cursor::new(&body);
    let (keys, _) =
        SignedPublicKey::from_armor_many(cursor).map_err(|e| SigningError::KeyParse(e.to_string()))?;

    let mut valid_keys = Vec::new();
    for key_result in keys {
        match key_result {
            Ok(key) => valid_keys.push(key),
            Err(e) => {
                tracing::warn!("skipping invalid GPG key for {}: {}", username, e);
            }
        }
    }

    if valid_keys.is_empty() {
        return Err(SigningError::KeyParse(format!(
            "no valid GPG keys found for {}",
            username
        )));
    }

    Ok(valid_keys)
}

/// Verify that a commit was signed by one of the expected signers.
///
/// Fetches GPG keys from GitHub for each signer and attempts to verify the
/// commit signature against each key (including subkeys).
///
/// Returns the username of the matching signer on success.
pub fn verify_commit_signature(
    repo_path: &Path,
    commit_hash: &str,
    expected_signers: &[String],
) -> Result<String, SigningError> {
    let (signature_armor, signed_payload) = extract_commit_signature(repo_path, commit_hash)?;

    // Parse the signature
    let cursor = Cursor::new(signature_armor.as_bytes());
    let (sig, _) = StandaloneSignature::from_armor_single(cursor)
        .map_err(|e| SigningError::SignatureParse(e.to_string()))?;

    // Create a blocking HTTP client for key fetching
    let client = reqwest::blocking::Client::builder()
        .user_agent("rattler-build")
        .build()
        .map_err(|e| SigningError::KeyFetch("".to_string(), e.to_string()))?;

    // Try each expected signer
    for signer in expected_signers {
        let keys = match fetch_github_gpg_keys(&client, signer) {
            Ok(keys) => keys,
            Err(e) => {
                tracing::warn!("failed to fetch keys for {}: {}", signer, e);
                continue;
            }
        };

        for key in &keys {
            // Try the primary key
            if sig
                .verify(&key, &signed_payload)
                .is_ok()
            {
                tracing::info!(
                    "commit {} verified as signed by {} (key {:?})",
                    commit_hash,
                    signer,
                    key.fingerprint()
                );
                return Ok(signer.clone());
            }

            // Try subkeys
            for subkey in &key.public_subkeys {
                if sig
                    .verify(subkey, &signed_payload)
                    .is_ok()
                {
                    tracing::info!(
                        "commit {} verified as signed by {} (subkey {:?})",
                        commit_hash,
                        signer,
                        subkey.fingerprint()
                    );
                    return Ok(signer.clone());
                }
            }
        }
    }

    Err(SigningError::VerificationFailed(
        expected_signers.to_vec(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_commit_signature_parsing() {
        // Test the signature extraction logic with a mock commit object.
        // In real git objects, all continuation lines in gpgsig start with a space.
        // The blank line separator in PGP armor appears as " " (just a space).
        let commit_content = [
            "tree abc123def456",
            "parent 789abc012def",
            "author Test User <test@example.com> 1234567890 +0000",
            "committer Test User <test@example.com> 1234567890 +0000",
            "gpgsig -----BEGIN PGP SIGNATURE-----",
            " ",
            " iQEzBAABCAAdFiEEtest",
            " =abcd",
            " -----END PGP SIGNATURE-----",
            "",
            "Initial commit",
        ]
        .join("\n");

        // Parse the gpgsig from the content
        let mut signature_lines = Vec::new();
        let mut payload_lines = Vec::new();
        let mut in_signature = false;

        for line in commit_content.lines() {
            if line.starts_with("gpgsig ") {
                in_signature = true;
                signature_lines.push(line.strip_prefix("gpgsig ").unwrap().to_string());
            } else if in_signature {
                if line.starts_with(' ') {
                    signature_lines.push(line[1..].to_string());
                    if line.contains("-----END PGP SIGNATURE-----") {
                        in_signature = false;
                    }
                } else {
                    in_signature = false;
                    payload_lines.push(line.to_string());
                }
            } else {
                payload_lines.push(line.to_string());
            }
        }

        assert!(!signature_lines.is_empty());
        assert!(signature_lines[0].contains("BEGIN PGP SIGNATURE"));
        assert!(signature_lines.last().unwrap().contains("END PGP SIGNATURE"));

        // Payload should not contain gpgsig
        let payload = payload_lines.join("\n");
        assert!(!payload.contains("gpgsig"));
        assert!(payload.contains("tree abc123def456"));
        assert!(payload.contains("Initial commit"));
    }
}
