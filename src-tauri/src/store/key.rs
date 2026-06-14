//! SQLCipher 密码管理：默认明文；启用加密时由用户在 Settings 里设置密码，
//! 密码缓存在 OS Keychain，下次启动透明打开。
//!
//! 测试旁路：环境变量 `WEFT_TEST_DB_PASSWORD` 存在时直接用它作密码，完全绕开
//! Keychain。集成测试搭配 `tempfile + WEFT_HOME + WEFT_TEST_DB_PASSWORD` 隔离环境。

use anyhow::Result;
use std::path::{Path, PathBuf};

const KEYCHAIN_SERVICE: &str = "weft";
const KEYCHAIN_ACCOUNT: &str = "db-password-v1";
const ENV_BYPASS: &str = "WEFT_TEST_DB_PASSWORD";

/// The canonical release data home (`~/.weft`). Its DB password keeps the bare
/// [`KEYCHAIN_ACCOUNT`] so existing encrypted installs need no re-entry.
fn default_release_home() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".weft"))
}

/// The active weft home, used to scope the DB credential. Surfaces the io error.
fn active_home() -> Result<PathBuf> {
    crate::paths::weft_home().map_err(|e| anyhow::anyhow!("weft_home: {e}"))
}

/// The Keychain account for the DB at `home`. Keyed to the *active home* — not the
/// build profile — because `WEFT_HOME` can point a debug build at the release DB
/// (or any relocated home); the credential must pair with the DB actually opened,
/// or an encrypted home gets read under the wrong account and fails to unlock. The
/// canonical release home keeps the bare account for backward compatibility.
fn keychain_account(home: &Path) -> String {
    keychain_account_for(home, default_release_home().as_deref())
}

fn keychain_account_for(home: &Path, release_home: Option<&Path>) -> String {
    if release_home == Some(home) {
        KEYCHAIN_ACCOUNT.to_string()
    } else {
        format!("{KEYCHAIN_ACCOUNT}::{}", home.display())
    }
}

/// A keyring handle for `account` under the weft service.
fn entry(account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, account).map_err(|e| anyhow::anyhow!("keyring entry: {e}"))
}

/// Read `account`'s stored password, mapping "no entry" to `None`.
fn read_account(account: &str) -> Result<Option<String>> {
    match entry(account)?.get_password() {
        Ok(pwd) => Ok(Some(pwd)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("keyring read: {e}")),
    }
}

/// Decide what `get_password` returns from the scoped-account lookup and (for a
/// non-bare active account) the legacy bare lookup, plus whether the chosen
/// password must be migrated (copied) into the scoped account. Before per-home
/// scoping, every home — including a relocated `WEFT_HOME` — stored its password
/// under the bare account, so a scoped home with no entry yet adopts the legacy
/// credential and copies it forward (a later release-side change can't desync it).
fn resolve_lookup(
    scoped: Option<String>,
    account_is_bare: bool,
    legacy_bare: Option<String>,
) -> Option<(String, bool)> {
    if let Some(pwd) = scoped {
        return Some((pwd, false));
    }
    if !account_is_bare {
        if let Some(legacy) = legacy_bare {
            return Some((legacy, true));
        }
    }
    None
}

/// 把用户密码序列化成 SQLCipher 的 `"x'<hex>'"` 字面量或带引号字符串字面量。
/// 密码走 PBKDF2，传 PRAGMA key 时用 SQL 字符串就行。注意密码里的单引号要 doubled。
pub fn pragma_literal(password: &str) -> String {
    let escaped = password.replace('\'', "''");
    format!("'{escaped}'")
}

/// The password stored under THIS home's own scoped account, or `None`. Env bypass
/// wins for tests. Deliberately does NO legacy fallback: it is the honest "does
/// this home have its own credential" signal that callers (recovery-key export,
/// snapshot) use to judge whether encryption is on for the active DB. A plaintext
/// home must read `None` here, never another home's credential. The legacy
/// bare-account fallback for opening a pre-scoping encrypted DB lives in
/// [`open_password`], gated on the encrypted-open path.
pub fn get_password() -> Result<Option<String>> {
    if let Ok(pwd) = std::env::var(ENV_BYPASS) {
        return Ok(Some(pwd));
    }
    read_account(&keychain_account(&active_home()?))
}

/// The password to open the encrypted DB at the active home. Tries this home's
/// scoped account; for a non-canonical home with no entry yet, adopts the legacy
/// bare credential (where pre-scoping versions stored every home's password) and
/// migrates it into the scoped account. ONLY the encrypted-DB open path calls
/// this — `get_password` is the side-effect-free read for everyone else, so a
/// plaintext home can never pull or migrate the release credential.
pub fn open_password() -> Result<Option<String>> {
    if let Ok(pwd) = std::env::var(ENV_BYPASS) {
        return Ok(Some(pwd));
    }
    let account = keychain_account(&active_home()?);
    let account_is_bare = account == KEYCHAIN_ACCOUNT;
    let scoped = read_account(&account)?;
    // Only consult the legacy bare account when this home has no entry of its own.
    let legacy_bare = if account_is_bare || scoped.is_some() {
        None
    } else {
        read_account(KEYCHAIN_ACCOUNT)?
    };
    match resolve_lookup(scoped, account_is_bare, legacy_bare) {
        Some((pwd, migrate)) => {
            if migrate {
                entry(&account)?
                    .set_password(&pwd)
                    .map_err(|e| anyhow::anyhow!("keyring migrate: {e}"))?;
            }
            Ok(Some(pwd))
        }
        None => Ok(None),
    }
}

/// 写密码到 Keychain（覆盖已有）。env bypass 模式下只设 process env var。
pub fn set_password(password: &str) -> Result<()> {
    if std::env::var(ENV_BYPASS).is_ok() {
        std::env::set_var(ENV_BYPASS, password);
        return Ok(());
    }
    let account = keychain_account(&active_home()?);
    entry(&account)?
        .set_password(password)
        .map_err(|e| anyhow::anyhow!("keyring write: {e}"))?;
    Ok(())
}

/// 删除 Keychain 里的密码条目；env bypass 模式下移除 env var。
/// 不存在不算错误（关闭加密时调用，本来就可能没设）。
pub fn delete_password() -> Result<()> {
    if std::env::var(ENV_BYPASS).is_ok() {
        std::env::remove_var(ENV_BYPASS);
        return Ok(());
    }
    let account = keychain_account(&active_home()?);
    match entry(&account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("keyring delete: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pragma_literal_quotes_password() {
        assert_eq!(pragma_literal("hello"), "'hello'");
    }

    #[test]
    fn pragma_literal_escapes_single_quote() {
        assert_eq!(pragma_literal("it's"), "'it''s'");
    }

    #[test]
    fn env_bypass_round_trips() {
        let prev = std::env::var(ENV_BYPASS).ok();
        std::env::set_var(ENV_BYPASS, "test-pw");
        assert_eq!(get_password().unwrap().as_deref(), Some("test-pw"));
        set_password("changed").unwrap();
        assert_eq!(get_password().unwrap().as_deref(), Some("changed"));
        delete_password().unwrap();
        // After delete in env-bypass mode the env var is gone.
        assert!(std::env::var(ENV_BYPASS).is_err());
        if let Some(v) = prev {
            std::env::set_var(ENV_BYPASS, v);
        }
    }

    #[test]
    fn release_home_keeps_bare_account() {
        // The installed app's home must keep the original account name, or existing
        // encrypted installs would look up the wrong credential and fail to unlock.
        let release = Path::new("/u/.weft");
        assert_eq!(keychain_account_for(release, Some(release)), "db-password-v1");
    }

    #[test]
    fn dev_and_relocated_homes_get_distinct_accounts() {
        // Dev's home and any WEFT_HOME-relocated home are keyed to that home, so a
        // debug build (or a relocated home) never reads or writes the release
        // credential — the account always pairs with the DB actually opened.
        let release = Some(Path::new("/u/.weft"));
        assert_eq!(
            keychain_account_for(Path::new("/u/.weft-dev"), release),
            "db-password-v1::/u/.weft-dev"
        );
        assert_eq!(
            keychain_account_for(Path::new("/custom/home"), release),
            "db-password-v1::/custom/home"
        );
    }

    #[test]
    fn scoped_password_wins_without_migration() {
        // When the home's own account already has a password, use it as-is — never
        // touch the legacy bare account.
        assert_eq!(
            resolve_lookup(Some("p".into()), false, Some("legacy".into())),
            Some(("p".to_string(), false))
        );
    }

    #[test]
    fn scoped_home_adopts_and_migrates_legacy_bare() {
        // A relocated/dev home with no entry yet adopts the legacy bare credential
        // and flags it for migration into its own account.
        assert_eq!(
            resolve_lookup(None, false, Some("legacy".into())),
            Some(("legacy".to_string(), true))
        );
    }

    #[test]
    fn bare_account_never_self_falls_back() {
        // The canonical release home IS the bare account, so it must not fall back
        // to itself; and with nothing stored anywhere the result is None (surfacing
        // the explicit "no password" error rather than a wrong credential).
        assert_eq!(resolve_lookup(None, true, Some("legacy".into())), None);
        assert_eq!(resolve_lookup(None, false, None), None);
    }
}
