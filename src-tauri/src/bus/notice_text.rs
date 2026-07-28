//! Human copy for the stable notice tokens background tasks post to the bus.
//!
//! Some notices are raised from detached tasks — a watchdog sweep, a
//! force-reset timer — that have no locale to render with: `lang` reaches the
//! backend per command invocation rather than being persisted. Those paths post
//! a TOKEN, and each consumer renders it with the locale it actually has.
//!
//! The webview owns the primary copy in `src/i18n/*.ts` (keyed through
//! `src/lib/noticeTokens.ts`) because it knows the operator's language. This
//! table exists for the consumers that cannot reach those catalogs — today the
//! IM bridge, which sends on its own fixed locale — so a remote human gets a
//! sentence instead of a raw `acp.force_reset_notice`.

/// The copy for `token`, or `None` when the text is ordinary agent prose that
/// should pass through untouched.
pub fn resolve(token: &str, lang: &str) -> Option<&'static str> {
    let zh = match token {
        crate::lead_chat::engine::ACP_FORCE_RESET_NOTICE => {
            "⏹️ 停止后 agent 未响应取消请求，已强制中断并重置为全新会话继续。历史对话仍保留在时间线里，但新会话不带原生上下文；如果后续回复像「忘记」了之前的内容，请重新提示一下关键信息。"
        }
        _ => return None,
    };
    if lang == "zh" {
        return Some(zh);
    }
    let en = match token {
        crate::lead_chat::engine::ACP_FORCE_RESET_NOTICE => {
            "⏹️ The agent did not answer the cancellation after Stop, so the turn was force-interrupted and continues on a brand-new session. The conversation stays in the timeline, but the new session carries no native context — if later replies seem to have \"forgotten\" earlier details, re-state the key points."
        }
        _ => return None,
    };
    Some(en)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tokens_resolve_in_both_languages() {
        for lang in ["zh", "en"] {
            let text = resolve(crate::lead_chat::engine::ACP_FORCE_RESET_NOTICE, lang)
                .expect("force-reset notice must have copy");
            assert!(!text.is_empty());
            assert!(
                !text.contains("acp.force_reset_notice"),
                "{lang}: the token must not leak into the copy"
            );
        }
    }

    /// Ordinary agent prose is not a token and must pass through untouched.
    #[test]
    fn prose_is_not_a_token() {
        assert!(resolve("Should I bump the major version?", "en").is_none());
        assert!(resolve("", "zh").is_none());
    }
}
