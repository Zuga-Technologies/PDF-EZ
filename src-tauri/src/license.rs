//! Offline license verification. The product promise: one online activation at
//! purchase, then the cached lease is verified by Ed25519 signature on every
//! startup with ZERO network calls, forever.
//!
//! Lease = small JSON blob minted server-side (Polar.sh webhook -> signer) with
//! a signature over a canonical payload string. Only the PUBLIC key ships in
//! the binary.

use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Base64 raw 32-byte Ed25519 public key. Dev key for now; swap for the
/// production signer's key before release (keys/license.pub).
const LICENSE_PUBKEY_B64: &str = include_str!("../keys/license.pub");

pub const LEASE_FILE: &str = "license.lease";

#[derive(Serialize, Deserialize, Clone)]
pub struct Lease {
    pub v: u8,
    pub product: String,
    pub email: String,
    pub order_id: String,
    /// RFC 3339 date the lease was minted.
    pub issued_at: String,
    /// Base64 Ed25519 signature over `canonical_payload()`.
    pub sig: String,
}

impl Lease {
    pub fn canonical_payload(&self) -> String {
        format!(
            "{}.v{}|{}|{}|{}",
            self.product, self.v, self.email, self.order_id, self.issued_at
        )
    }
}

#[derive(Serialize, Clone)]
pub struct LicenseStatus {
    pub licensed: bool,
    pub email: Option<String>,
    pub dev_build: bool,
    pub reason: Option<String>,
}

fn verifying_key() -> Result<VerifyingKey, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(LICENSE_PUBKEY_B64.trim())
        .map_err(|e| format!("bad embedded public key: {e}"))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "embedded public key is not 32 bytes".to_string())?;
    VerifyingKey::from_bytes(&arr).map_err(|e| format!("bad embedded public key: {e}"))
}

pub fn verify_lease(lease: &Lease) -> Result<(), String> {
    if lease.product != "zugapdf" {
        return Err("lease is for a different product".to_string());
    }
    let key = verifying_key()?;
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(lease.sig.trim())
        .map_err(|_| "malformed signature".to_string())?;
    let sig = Signature::from_slice(&sig_bytes).map_err(|_| "malformed signature".to_string())?;
    key.verify(lease.canonical_payload().as_bytes(), &sig)
        .map_err(|_| "signature check failed".to_string())
}

pub fn lease_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(LEASE_FILE)
}

pub fn load_lease(app_data_dir: &Path) -> Option<Lease> {
    let raw = fs::read_to_string(lease_path(app_data_dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Startup check. Debug builds run unlicensed so development has zero friction;
/// release builds enforce the lease.
pub fn check(app_data_dir: &Path) -> LicenseStatus {
    match load_lease(app_data_dir) {
        Some(lease) => match verify_lease(&lease) {
            Ok(()) => LicenseStatus {
                licensed: true,
                email: Some(lease.email),
                dev_build: cfg!(debug_assertions),
                reason: None,
            },
            Err(e) => LicenseStatus {
                licensed: cfg!(debug_assertions),
                email: None,
                dev_build: cfg!(debug_assertions),
                reason: Some(e),
            },
        },
        None => LicenseStatus {
            licensed: cfg!(debug_assertions),
            email: None,
            dev_build: cfg!(debug_assertions),
            reason: Some("no license on this machine".to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn dev_signed_lease(email: &str, order_id: &str) -> Option<Lease> {
        // Uses the local dev keypair; skip silently on machines without it.
        let key_b64 =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/keys/license.key"))
                .ok()?;
        let seed: [u8; 32] = base64::engine::general_purpose::STANDARD
            .decode(key_b64.trim())
            .ok()?
            .try_into()
            .ok()?;
        let signing = SigningKey::from_bytes(&seed);
        let mut lease = Lease {
            v: 1,
            product: "zugapdf".into(),
            email: email.into(),
            order_id: order_id.into(),
            issued_at: "2026-07-05T00:00:00Z".into(),
            sig: String::new(),
        };
        lease.sig = base64::engine::general_purpose::STANDARD
            .encode(signing.sign(lease.canonical_payload().as_bytes()).to_bytes());
        Some(lease)
    }

    #[test]
    fn valid_lease_verifies() {
        let Some(lease) = dev_signed_lease("buyer@example.com", "ord-1") else { return };
        assert!(verify_lease(&lease).is_ok());
    }

    #[test]
    fn tampered_email_fails() {
        let Some(mut lease) = dev_signed_lease("buyer@example.com", "ord-1") else { return };
        lease.email = "pirate@example.com".into();
        assert!(verify_lease(&lease).is_err());
    }

    #[test]
    fn wrong_product_fails() {
        let Some(mut lease) = dev_signed_lease("buyer@example.com", "ord-1") else { return };
        lease.product = "zugapdf-pro".into();
        assert!(verify_lease(&lease).is_err());
    }

    #[test]
    fn garbage_signature_fails() {
        let Some(mut lease) = dev_signed_lease("buyer@example.com", "ord-1") else { return };
        lease.sig = "AAAA".into();
        assert!(verify_lease(&lease).is_err());
    }
}

/// Import a lease file (the activation path until the Polar.sh online flow
/// lands; stays forever as the offline/airgapped activation route).
pub fn activate_from_file(app_data_dir: &Path, lease_file: &Path) -> Result<LicenseStatus, String> {
    let raw = fs::read_to_string(lease_file).map_err(|e| format!("could not read file: {e}"))?;
    let lease: Lease =
        serde_json::from_str(&raw).map_err(|_| "not a valid ZugaPDF license file".to_string())?;
    verify_lease(&lease)?;
    fs::create_dir_all(app_data_dir).map_err(|e| format!("could not store license: {e}"))?;
    fs::write(lease_path(app_data_dir), &raw).map_err(|e| format!("could not store license: {e}"))?;
    Ok(LicenseStatus {
        licensed: true,
        email: Some(lease.email),
        dev_build: cfg!(debug_assertions),
        reason: None,
    })
}
