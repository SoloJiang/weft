//! Test-case document helpers. The document itself is a markdown tree
//! (`#` title + nested unordered lists; leaves are individual cases) stored in
//! the `test_plan` table — the chat timeline only carries a summary card, built
//! here. Pure string parsing, unit-tested in place.

/// Build the summary-card payload for a test-case markdown tree:
/// `{"title", "branches": [..], "caseCount"}`.
///
/// - `title`: first `# ` heading (empty when none — the UI falls back to i18n).
/// - `branches`: top-level groupings in encounter order, capped at 6. When the
///   document has `## ` headings those ARE the groups; only a heading-less
///   document falls back to zero-indent list items (leads write either shape).
/// - `caseCount`: leaf list items (no deeper list item directly below), i.e.
///   individual cases rather than groupings.
pub fn summarize(md: &str) -> serde_json::Value {
    let mut title = String::new();
    let mut heading_branches: Vec<String> = Vec::new();
    let mut bullet_branches: Vec<String> = Vec::new();
    // (indent, text) for every list item, in order — leaves resolved after.
    let mut items: Vec<(usize, String)> = Vec::new();

    for line in md.lines() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if let Some(h) = trimmed.strip_prefix("# ") {
            if title.is_empty() {
                title = h.trim().to_string();
            }
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("## ") {
            heading_branches.push(h.trim().to_string());
            continue;
        }
        let bullet = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "));
        if let Some(text) = bullet {
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            if indent == 0 {
                bullet_branches.push(text.clone());
            }
            items.push((indent, text));
        }
    }
    let mut branches = if heading_branches.is_empty() {
        bullet_branches
    } else {
        heading_branches
    };

    let case_count = items
        .iter()
        .enumerate()
        .filter(|(i, (indent, _))| {
            // A leaf has no deeper list item directly below it.
            items.get(i + 1).is_none_or(|(next, _)| next <= indent)
        })
        .count();

    branches.dedup();
    branches.truncate(6);
    serde_json::json!({
        "title": title,
        "branches": branches,
        "caseCount": case_count,
    })
}

#[cfg(test)]
mod tests {
    use super::summarize;

    #[test]
    fn summarizes_title_branches_and_leaf_count() {
        let md = "# 登录功能测试\n\n## 正常路径\n- 密码登录\n  - 正确密码成功\n  - 记住我保持会话\n## 异常路径\n- 密码错误\n  - 三次失败锁定\n- 网络超时\n";
        let v = summarize(md);
        assert_eq!(v["title"], "登录功能测试");
        let branches: Vec<&str> = v["branches"]
            .as_array()
            .expect("branches array")
            .iter()
            .filter_map(|b| b.as_str())
            .collect();
        assert_eq!(branches, vec!["正常路径", "异常路径"]);
        // Leaves: 正确密码成功, 记住我保持会话, 三次失败锁定, 网络超时.
        assert_eq!(v["caseCount"], 4);
    }

    #[test]
    fn zero_indent_bullets_count_as_branches_without_headings() {
        let md = "- 边界\n  - 空输入\n- 并发\n  - 双击提交\n  - 竞态覆盖\n";
        let v = summarize(md);
        assert_eq!(v["title"], "");
        assert_eq!(v["branches"].as_array().map(|a| a.len()), Some(2));
        assert_eq!(v["caseCount"], 3);
    }

    #[test]
    fn flat_list_counts_every_item_as_a_case() {
        let md = "# T\n- a\n- b\n- c\n";
        let v = summarize(md);
        assert_eq!(v["caseCount"], 3);
        assert_eq!(v["branches"].as_array().map(|a| a.len()), Some(3));
    }

    #[test]
    fn branches_cap_at_six_and_empty_doc_is_zero() {
        let md = "- 1\n- 2\n- 3\n- 4\n- 5\n- 6\n- 7\n";
        let v = summarize(md);
        assert_eq!(v["branches"].as_array().map(|a| a.len()), Some(6));
        let empty = summarize("");
        assert_eq!(empty["caseCount"], 0);
        assert_eq!(empty["title"], "");
    }
}
