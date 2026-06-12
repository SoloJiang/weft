//! Recovery Key file format: plain JSON the user backs up themselves. The
//! file holds the SQLCipher key as base64 so a user with this file + the
//! backup git repo can decrypt their data on a fresh machine. Spec §4.

use anyhow::{Result, anyhow};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::store::key::SqlCipherKey;

const FORMAT_VERSION: u32 = 1;
const KEYCHAIN_SERVICE: &str = "weft";
const KEYCHAIN_ACCOUNT: &str = "db-key-v1";

#[derive(Debug, Serialize, Deserialize)]
struct RecoveryKeyFile {
    version: u32,
    service: String,
    account: String,
    key_b64: String,
    exported_at: String,
    note: String,
}

const NOTE: &str =
    "Keep this file safe. Anyone with this file AND your backup repo can decrypt your Weft data.";

/// Read the live key out of the Keychain (or env bypass) and write it to
/// `target` (must not exist) as pretty-printed JSON.
pub fn export_to(target: &Path) -> Result<()> {
    if target.exists() {
        return Err(anyhow!(
            "recovery key target already exists: {}",
            target.display()
        ));
    }
    let k = crate::store::key::get_or_create()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(k.to_bytes());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".into());

    let rec = RecoveryKeyFile {
        version: FORMAT_VERSION,
        service: KEYCHAIN_SERVICE.into(),
        account: KEYCHAIN_ACCOUNT.into(),
        key_b64: b64,
        exported_at: now,
        note: NOTE.into(),
    };
    std::fs::write(target, serde_json::to_vec_pretty(&rec)?)?;
    Ok(())
}

/// Read `source`, validate format, and write the key back into the Keychain
/// (overwriting any existing entry). When `WEFT_TEST_DB_KEY_B64` is set, we
/// only validate format and return the parsed key without touching the OS
/// Keychain — same bypass policy as `store::key`.
pub fn import_from(source: &Path) -> Result<SqlCipherKey> {
    let bytes = std::fs::read(source)
        .map_err(|e| anyhow!("read recovery key {}: {e}", source.display()))?;
    let rec: RecoveryKeyFile = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow!("parse recovery key {}: {e}", source.display()))?;
    if rec.version != FORMAT_VERSION {
        return Err(anyhow!(
            "unsupported recovery key version: {} (expected {})",
            rec.version,
            FORMAT_VERSION
        ));
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(rec.key_b64.trim())
        .map_err(|e| anyhow!("decode key_b64: {e}"))?;
    let key = SqlCipherKey::from_bytes(&raw)?;

    if std::env::var("WEFT_TEST_DB_KEY_B64").is_ok() {
        return Ok(key);
    }

    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| anyhow!("keyring entry: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(key.to_bytes());
    entry
        .set_password(&b64)
        .map_err(|e| anyhow!("keyring write: {e}"))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn iso_key_env() {
        let raw = [0x77u8; 48];
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        std::env::set_var("WEFT_TEST_DB_KEY_B64", &b64);
    }

    #[test]
    fn export_then_import_roundtrip() {
        let _g = crate::backup::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        iso_key_env();
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rk.json");
        export_to(&p).unwrap();
        let imported = import_from(&p).unwrap();

        let original = crate::store::key::get_or_create().unwrap();
        assert_eq!(imported.to_bytes(), original.to_bytes());
    }

    #[test]
    fn rejects_existing_export_target() {
        let _g = crate::backup::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        iso_key_env();
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rk.json");
        std::fs::write(&p, b"{}").unwrap();
        assert!(export_to(&p).is_err());
    }

    #[test]
    fn rejects_unknown_version() {
        let _g = crate::backup::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        iso_key_env();
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rk.json");
        std::fs::write(
            &p,
            br#"{"version":99,"service":"weft","account":"db-key-v1","key_b64":"AA==","exported_at":"0","note":""}"#,
        )
        .unwrap();
        assert!(import_from(&p).is_err());
    }
}
