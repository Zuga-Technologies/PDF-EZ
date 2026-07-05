//! User settings, persisted as `settings.json` in the app data directory.
//!
//! The whole point of this module is the network contract: ZugaPDF is offline
//! by default and the ONLY features that may ever touch the network are the two
//! flags below — both off until the user turns them on. Every online action
//! must pass [`require`] first, so a feature can never quietly dial out.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
pub struct Settings {
    /// Allow ONE outbound call to an RFC 3161 timestamp authority when signing
    /// (upgrades a signature from B-B to B-T). Off by default.
    #[serde(default)]
    pub allow_trusted_timestamp: bool,
    /// Allow OCSP / CRL revocation checks when verifying a signature. Off by
    /// default — offline verification still checks document integrity, the
    /// cryptography, and the certificate chain math.
    #[serde(default)]
    pub allow_online_verification: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { allow_trusted_timestamp: false, allow_online_verification: false }
    }
}

/// The two — and only two — features permitted to go online.
pub enum OnlineFeature {
    TrustedTimestamp,
    OnlineVerification,
}

impl OnlineFeature {
    fn label(&self) -> &'static str {
        match self {
            OnlineFeature::TrustedTimestamp => "Trusted timestamp",
            OnlineFeature::OnlineVerification => "Online verification",
        }
    }
}

fn file(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

pub fn load(dir: &Path) -> Settings {
    std::fs::read(file(dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(dir: &Path, settings: &Settings) -> Result<Settings, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(file(dir), json).map_err(|e| e.to_string())?;
    Ok(settings.clone())
}

/// The gate every online action must pass. Returns `Err` — blocking the call —
/// unless the user has switched that feature on. No online feature exists yet;
/// the timestamp and verification code paths will call this before they dial.
#[allow(dead_code)]
pub fn require(dir: &Path, feature: OnlineFeature) -> Result<(), String> {
    let s = load(dir);
    let allowed = match feature {
        OnlineFeature::TrustedTimestamp => s.allow_trusted_timestamp,
        OnlineFeature::OnlineVerification => s.allow_online_verification,
    };
    if allowed {
        Ok(())
    } else {
        Err(format!("{} is turned off — enable it in Settings to allow this one online step.", feature.label()))
    }
}
