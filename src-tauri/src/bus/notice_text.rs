//! Human copy for the stable notice tokens background tasks post to the bus.
//!
//! Some notices are raised from detached tasks — a watchdog sweep, a
//! force-reset timer — that have no locale to render with: `lang` reaches the
//! backend per command invocation rather than being persisted. Those paths post
//! a TOKEN, and each consumer renders it with the locale it actually has.
//!
//! The webview renders tokens through its own catalogs. This module exists for
//! consumers that cannot reach them — today the IM bridge, which sends on its
//! own fixed locale.
//!
//! The copy is AUTHORED in `src/i18n/{en,zh}.ts` like every other user-facing
//! string (AGENTS.md), and `scripts/gen-notice-copy.mjs` mirrors the tokens
//! listed there into `notices.generated.json`, which this module bakes in at
//! compile time. Rust never authors the sentences — an earlier version did, and
//! that meant a catalog edit silently gave remote and in-app users different
//! text. `tests/frontend/noticeCopy.test.ts` fails if the mirror drifts from
//! the catalogs, so the generated file cannot go stale unnoticed.

use std::collections::HashMap;
use std::sync::LazyLock;

/// `token -> lang -> copy`, compiled in from the generated catalog mirror.
///
/// A malformed file yields an EMPTY table rather than a panic (production paths
/// must not panic), which would surface as raw tokens. The test below fails the
/// build's test run in that case, so it cannot ship silently.
static NOTICES: LazyLock<HashMap<String, HashMap<String, String>>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("notices.generated.json")).unwrap_or_default()
});

/// The copy for `token` in `lang`, or `None` when the text is ordinary agent
/// prose that should pass through untouched.
///
/// Falls back to English for a locale the catalog does not carry — a sentence
/// in the wrong language still beats a raw `acp.force_reset_notice`.
pub fn resolve(token: &str, lang: &str) -> Option<&'static str> {
    let by_lang = NOTICES.get(token)?;
    by_lang
        .get(lang)
        .or_else(|| by_lang.get("en"))
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is `include_str!`-ed, so a malformed or moved file degrades to
    /// empty at runtime. This is the check that keeps that from shipping.
    #[test]
    fn generated_notice_copy_parses_and_carries_both_languages() {
        assert!(
            !NOTICES.is_empty(),
            "notices.generated.json failed to parse — every token would render raw"
        );
        for (token, by_lang) in NOTICES.iter() {
            for lang in ["en", "zh"] {
                let copy = by_lang
                    .get(lang)
                    .unwrap_or_else(|| panic!("{token} is missing {lang}"));
                assert!(!copy.trim().is_empty(), "{token}/{lang} is blank");
                assert!(
                    !copy.contains(token),
                    "{token}/{lang} contains the raw token"
                );
            }
        }
    }

    /// The one token the force-reset path posts must be renderable, or a remote
    /// human receives `acp.force_reset_notice` verbatim.
    #[test]
    fn the_force_reset_token_resolves_in_both_languages() {
        for lang in ["zh", "en"] {
            let text = resolve(crate::lead_chat::engine::ACP_FORCE_RESET_NOTICE, lang)
                .expect("force-reset notice must have copy");
            assert!(!text.is_empty());
        }
    }

    /// An unknown locale still gets a sentence rather than the token.
    #[test]
    fn an_unknown_locale_falls_back_to_english() {
        let text = resolve(crate::lead_chat::engine::ACP_FORCE_RESET_NOTICE, "fr")
            .expect("fallback copy");
        let en = resolve(crate::lead_chat::engine::ACP_FORCE_RESET_NOTICE, "en").expect("en copy");
        assert_eq!(text, en);
    }

    /// Ordinary agent prose is not a token and must pass through untouched.
    #[test]
    fn prose_is_not_a_token() {
        assert!(resolve("Should I bump the major version?", "en").is_none());
        assert!(resolve("", "zh").is_none());
    }
}
