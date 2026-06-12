//! Recovery Key file format (v2): plain JSON the user backs up themselves.
//! The file holds the user's SQLCipher password so a user with this file + the
//! backup git repo can decrypt their data on a fresh machine. Spec §4.
//!
//! v1 (binary 48-byte key, base64 in `key_b64`) is rejected — the auto Keychain
//! key path is gone and pre-launch we have no v1 users to migrate.

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

const FORMAT_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
struct RecoveryKeyFile {
    version: u32,
    /// The user's password. Encrypted backups can't be decrypted without it.
    password: String,
    exported_at: String,
    note: String,
}

const NOTE: &str =
    "Keep this file safe. Anyone with this file AND your backup repo can decrypt your Weft data.";

/// Read the live password out of the Keychain and write it to `target` (must
/// not exist) as pretty-printed JSON. Errors out if encryption is not on
/// (no password to export).
pub fn export_to(target: &Path) -> Result<()> {
    if target.exists() {
        return Err(anyhow!(
            "recovery key target already exists: {}",
            target.display()
        ));
    }
    let pwd = crate::store::key::get_password()?
        .ok_or_else(|| anyhow!("no password in keychain — enable encryption first"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".into());

    let rec = RecoveryKeyFile {
        version: FORMAT_VERSION,
        password: pwd,
        exported_at: now,
        note: NOTE.into(),
    };
    std::fs::write(target, serde_json::to_vec_pretty(&rec)?)?;
    Ok(())
}

/// Read `source`, validate format, return the password. Caller decides what to
/// do with it (typically `store::key::set_password` on the restoring machine).
pub fn import_from(source: &Path) -> Result<String> {
    let bytes = std::fs::read(source)
        .map_err(|e| anyhow!("read recovery key {}: {e}", source.display()))?;
    let rec: RecoveryKeyFile = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow!("parse recovery key {}: {e}", source.display()))?;
    if rec.version != FORMAT_VERSION {
        bail!(
            "unsupported recovery key version: {} (expected {})",
            rec.version,
            FORMAT_VERSION
        );
    }
    if rec.password.is_empty() {
        bail!("recovery key has empty password");
    }
    Ok(rec.password)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_then_import_roundtrip() {
        let _g = crate::backup::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("WEFT_TEST_DB_PASSWORD", "sekret");

        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rk.json");
        export_to(&p).unwrap();
        let imported = import_from(&p).unwrap();
        assert_eq!(imported, "sekret");

        std::env::remove_var("WEFT_TEST_DB_PASSWORD");
    }

    #[test]
    fn rejects_existing_export_target() {
        let _g = crate::backup::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("WEFT_TEST_DB_PASSWORD", "sekret");

        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rk.json");
        std::fs::write(&p, b"{}").unwrap();
        assert!(export_to(&p).is_err());

        std::env::remove_var("WEFT_TEST_DB_PASSWORD");
    }

    #[test]
    fn rejects_v1_format() {
        let _g = crate::backup::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rk.json");
        std::fs::write(
            &p,
            br#"{"version":1,"service":"weft","account":"db-key-v1","key_b64":"AA==","exported_at":"0","note":""}"#,
        )
        .unwrap();
        assert!(import_from(&p).is_err());
    }

    #[test]
    fn rejects_empty_password() {
        let _g = crate::backup::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rk.json");
        std::fs::write(
            &p,
            br#"{"version":2,"password":"","exported_at":"0","note":""}"#,
        )
        .unwrap();
        assert!(import_from(&p).is_err());
    }
}
