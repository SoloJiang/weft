//! Data-home resolution for weft-codex. Honors `WEFT_CODEX_HOME` (test
//! isolation and dev/release splits), defaulting to `~/.weft-codex` — the new
//! project deliberately does NOT share weft's `~/.weft`.

use std::path::PathBuf;

pub fn weft_home() -> std::io::Result<PathBuf> {
    let dir = match std::env::var("WEFT_CODEX_HOME") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var("HOME").map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::NotFound, error)
            })?;
            PathBuf::from(home).join(".weft-codex")
        }
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
