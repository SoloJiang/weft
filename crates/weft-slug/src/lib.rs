//! Slugs used in filesystem paths, git branch names, and task ids.
//!
//! Two shapes:
//! - [`slugify`] / [`unique_slug`] — lowercase ASCII, safe for weft paths/branches.
//! - [`task_slug`] — keeps CJK so weft-codex `task_create` names stay readable.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

/// Lowercase, replace any run of non-[a-z0-9] with a single '-', trim hyphens.
/// Empty input (or input with no usable chars) yields "item".
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in name.chars() {
        let lc = c.to_ascii_lowercase();
        if lc.is_ascii_alphanumeric() {
            out.push(lc);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "item".to_string()
    } else {
        trimmed
    }
}

/// slugify(name), then ensure uniqueness against `existing` by appending
/// "-2", "-3", ... until free.
pub fn unique_slug(name: &str, existing: &[String]) -> String {
    let base = slugify(name);
    if !existing.iter().any(|e| e == &base) {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !existing.iter().any(|e| e == &candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// weft-codex task slug: ASCII alphanumerics plus CJK, max 48 chars, fallback "task".
pub fn task_slug(name: &str) -> String {
    let mut slug = String::new();
    let mut length = 0;
    let mut separator_pending = false;
    for character in name.chars().flat_map(char::to_lowercase) {
        let is_cjk = ('\u{4e00}'..='\u{9fff}').contains(&character);
        if character.is_ascii_alphanumeric() || is_cjk {
            if separator_pending && !slug.is_empty() {
                if length + 1 >= 48 {
                    break;
                }
                slug.push('-');
                length += 1;
            }
            separator_pending = false;
            if length >= 48 {
                break;
            }
            slug.push(character);
            length += 1;
        } else if !slug.is_empty() {
            separator_pending = true;
        }
    }
    if slug.is_empty() {
        "task".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("My Feature"), "my-feature");
        assert_eq!(slugify("web-app/.git"), "web-app-git");
        assert_eq!(slugify("  Hello   World  "), "hello-world");
        assert_eq!(slugify("café & co"), "caf-co");
        assert_eq!(slugify("!!!"), "item");
        assert_eq!(slugify(""), "item");
    }

    #[test]
    fn unique_slug_dedups() {
        let existing = vec!["api".to_string(), "api-2".to_string()];
        assert_eq!(unique_slug("API", &existing), "api-3");
        assert_eq!(unique_slug("fresh", &existing), "fresh");
    }

    #[test]
    fn task_slug_preserves_cjk_and_has_no_trailing_separator() {
        assert_eq!(task_slug("修复 Login 回调"), "修复-login-回调");
        assert!(!task_slug(&format!("{} end", "x".repeat(60))).ends_with('-'));
        assert!(!task_slug("***").is_empty());
        assert_eq!(task_slug("***"), "task");
    }
}
