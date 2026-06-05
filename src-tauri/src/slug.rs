//! Slugs used in both filesystem paths and git branch names. Must be safe for
//! both: lowercase ASCII, digits, single hyphens; no leading/trailing hyphen;
//! never empty; de-duplicated against existing siblings.

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
}
