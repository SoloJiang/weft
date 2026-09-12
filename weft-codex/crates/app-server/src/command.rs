//! Resolve the only supported agent executable: Codex.

/// Codex executable or absolute path. Arguments are deliberately not accepted;
/// app-server arguments are constructed by the typed client.
pub fn codex_command() -> String {
    std::env::var("WEFT_CODEX_COMMAND")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "codex".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_command_is_codex() {
        if std::env::var_os("WEFT_CODEX_COMMAND").is_none() {
            assert_eq!(codex_command(), "codex");
        }
    }
}
