//! Offline encryption lifecycle helpers (enable / disable / change password).
//!
//! All three operations:
//!   1. open a connection to the current `weft.db` (plain or encrypted as appropriate)
//!   2. write a fresh-shape copy to `weft.db.encrypt-tmp` using `sqlcipher_export`
//!   3. close the source connection
//!   4. atomically replace `weft.db` with the tmp file
//!   5. update the Keychain password entry
//!
//! Caller is expected to (a) hold the global Db handle and `drop` / replace it
//! around the swap (this module does not touch app state), and (b) trigger a
//! restart so worker tasks reattach to the new file.
//!
//! WAL sidecars (`weft.db-wal` / `weft.db-shm`) are removed after the swap so
//! the new file starts with a clean WAL. We never edit the source file
//! in-place; if anything fails mid-swap the original is untouched.

use anyhow::{anyhow, bail, Result};
use sea_orm::ConnectionTrait;
use std::path::Path;

use crate::store::{detect_encrypted, key};

const TMP_SUFFIX: &str = "encrypt-tmp";

/// Enable encryption on a currently-plaintext `weft.db`. `new_password` is the
/// password the user just chose. On success the on-disk file is encrypted and
/// the password is persisted to the Keychain.
pub async fn enable(db_path: &Path, new_password: &str) -> Result<()> {
    if new_password.is_empty() {
        bail!("password must not be empty");
    }
    if !db_path.exists() {
        bail!("weft.db not found at {}", db_path.display());
    }
    if detect_encrypted(db_path)? {
        bail!("weft.db is already encrypted");
    }
    let tmp = sibling_tmp(db_path)?;
    if tmp.exists() {
        std::fs::remove_file(&tmp)?;
    }
    rekey_via_export(
        db_path,
        /*src_password*/ None,
        &tmp,
        Some(new_password),
    )
    .await?;
    finalize_swap(db_path, &tmp)?;
    key::set_password(new_password)?;
    Ok(())
}

/// Decrypt a currently-encrypted `weft.db` back to plaintext. `current_password`
/// must match. On success the password is removed from the Keychain.
pub async fn disable(db_path: &Path, current_password: &str) -> Result<()> {
    if current_password.is_empty() {
        bail!("current password required");
    }
    if !db_path.exists() {
        bail!("weft.db not found at {}", db_path.display());
    }
    if !detect_encrypted(db_path)? {
        bail!("weft.db is not encrypted");
    }
    let tmp = sibling_tmp(db_path)?;
    if tmp.exists() {
        std::fs::remove_file(&tmp)?;
    }
    rekey_via_export(db_path, Some(current_password), &tmp, /*dst pwd*/ None).await?;
    finalize_swap(db_path, &tmp)?;
    key::delete_password()?;
    Ok(())
}

/// Re-key an encrypted `weft.db` from `old_password` to `new_password`. Wrong
/// `old_password` surfaces as `Err` (the source ATTACH will fail to read).
pub async fn change_password(db_path: &Path, old_password: &str, new_password: &str) -> Result<()> {
    if old_password.is_empty() {
        bail!("old password required");
    }
    if new_password.is_empty() {
        bail!("new password required");
    }
    if !db_path.exists() {
        bail!("weft.db not found at {}", db_path.display());
    }
    if !detect_encrypted(db_path)? {
        bail!("weft.db is not encrypted");
    }
    let tmp = sibling_tmp(db_path)?;
    if tmp.exists() {
        std::fs::remove_file(&tmp)?;
    }
    rekey_via_export(db_path, Some(old_password), &tmp, Some(new_password)).await?;
    finalize_swap(db_path, &tmp)?;
    key::set_password(new_password)?;
    Ok(())
}

/// Open `src` (with `src_password` if encrypted, else plain), ATTACH `dst` (with
/// `dst_password` if Some), export, close. `dst` must not exist on entry.
async fn rekey_via_export(
    src: &Path,
    src_password: Option<&str>,
    dst: &Path,
    dst_password: Option<&str>,
) -> Result<()> {
    let src_url = format!("sqlite://{}?mode=rwc", src.to_string_lossy());
    let conn = if let Some(pwd) = src_password {
        let mut opt = sea_orm::ConnectOptions::new(src_url);
        opt.sqlcipher_key(key::pragma_literal(pwd));
        sea_orm::Database::connect(opt).await?
    } else {
        sea_orm::Database::connect(src_url).await?
    };

    // Probe the source so we fail fast on wrong password (otherwise ATTACH
    // succeeds and sqlcipher_export trips on read).
    conn.execute_unprepared("SELECT count(*) FROM sqlite_master;")
        .await
        .map_err(|e| anyhow!("source open failed (wrong password?): {e}"))?;

    let dst_str = dst.to_string_lossy().replace('\'', "''");
    let attach = match dst_password {
        Some(pwd) => format!(
            "ATTACH DATABASE '{}' AS rekey KEY {};",
            dst_str,
            key::pragma_literal(pwd)
        ),
        None => format!("ATTACH DATABASE '{}' AS rekey KEY '';", dst_str),
    };

    let r = async {
        conn.execute_unprepared(&attach).await?;
        conn.execute_unprepared("SELECT sqlcipher_export('rekey');")
            .await?;
        conn.execute_unprepared("DETACH DATABASE rekey;").await?;
        Ok::<(), sea_orm::DbErr>(())
    }
    .await;

    // Always close the source pool before any rename. SeaORM/sqlx pools don't
    // expose explicit close; drop is what flushes WAL. close_owned avoids the
    // background ping keeping it alive.
    let _ = conn.close().await;

    if let Err(e) = r {
        if dst.exists() {
            let _ = std::fs::remove_file(dst);
        }
        return Err(anyhow!("rekey failed: {e}"));
    }
    Ok(())
}

fn sibling_tmp(db_path: &Path) -> Result<std::path::PathBuf> {
    let parent = db_path
        .parent()
        .ok_or_else(|| anyhow!("db path has no parent: {}", db_path.display()))?;
    let name = db_path
        .file_name()
        .ok_or_else(|| anyhow!("db path has no file name: {}", db_path.display()))?
        .to_string_lossy()
        .into_owned();
    Ok(parent.join(format!("{name}.{TMP_SUFFIX}")))
}

/// Replace `db_path` with `tmp`; clean stale WAL sidecars so the new file
/// starts fresh. Atomic on the same filesystem (rename is POSIX-atomic).
fn finalize_swap(db_path: &Path, tmp: &Path) -> Result<()> {
    // remove WAL sidecars from the old db first; they belong to the old key.
    for ext in ["-wal", "-shm"] {
        let side = db_path.with_extension(format!(
            "{}{ext}",
            db_path.extension().and_then(|s| s.to_str()).unwrap_or("db")
        ));
        if side.exists() {
            let _ = std::fs::remove_file(&side);
        }
    }
    // and the appended-style sidecars (`weft.db-wal` next to `weft.db`).
    let parent = db_path
        .parent()
        .ok_or_else(|| anyhow!("db path has no parent"))?;
    let stem = db_path
        .file_name()
        .ok_or_else(|| anyhow!("db path has no file name"))?
        .to_string_lossy()
        .into_owned();
    for ext in ["-wal", "-shm"] {
        let p = parent.join(format!("{stem}{ext}"));
        if p.exists() {
            let _ = std::fs::remove_file(&p);
        }
    }
    std::fs::rename(tmp, db_path).map_err(|e| {
        anyhow!(
            "swap rename failed ({} -> {}): {e}",
            tmp.display(),
            db_path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_tmp_appends_suffix() {
        let p = std::path::Path::new("/tmp/x/weft.db");
        let s = sibling_tmp(p).unwrap();
        assert_eq!(s, std::path::Path::new("/tmp/x/weft.db.encrypt-tmp"));
    }
}
