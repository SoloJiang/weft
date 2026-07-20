//! Scan assistant text for weft control sentinels so the engine can fork them
//! out of the timeline body. Pure string scanning — no regex dep, no allocs
//! beyond the cleaned output. Four markers today, matching the directives
//! injected via `commands` (`SENTINEL_DIRECTIVES` / `PLAN_CARD_DIRECTIVES` /
//! `TEST_CASES_DIRECTIVES`):
//!   `<weft:action_card>{json}</weft:action_card>` — assistant proposes a repo-onboarding card.
//!   `<weft:plan_card>{json}</weft:plan_card>` — assistant proposes the issue plan for confirmation.
//!   `<weft:test_cases>markdown</weft:test_cases>` — the issue's test-case tree (RAW markdown, not JSON).
//!   `<weft:list_repos/>` — assistant requests the current workspace's repos.
//! Malformed (unclosed) cards stay inline as plain text so a half-typed
//! sentinel never silently swallows assistant output.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sentinel {
    /// Raw JSON payload (the text between the open and close tags).
    ActionCard(String),
    /// Raw JSON payload of the lead's plan card (the discuss-first gate).
    PlanCard(String),
    /// Raw MARKDOWN body (not JSON): the issue's test-case tree. Multi-line
    /// markdown inside a JSON string invites escaping mistakes from the model,
    /// so this sentinel carries the document verbatim.
    TestCases(String),
    ListRepos,
}

const OPEN_AC: &str = "<weft:action_card>";
const CLOSE_AC: &str = "</weft:action_card>";
const OPEN_PC: &str = "<weft:plan_card>";
const CLOSE_PC: &str = "</weft:plan_card>";
const OPEN_TC: &str = "<weft:test_cases>";
const CLOSE_TC: &str = "</weft:test_cases>";
const LIST_REPOS: &str = "<weft:list_repos/>";

#[derive(Clone, Copy)]
enum Kind {
    ActionCard,
    PlanCard,
    TestCases,
    ListRepos,
}

/// Scan `text` left-to-right; returns the cleaned body (sentinels stripped) and
/// the sentinels in encounter order. Unknown `<…/>` tags and unclosed cards are
/// left in the body verbatim. Lead semantics (all markers active).
pub fn extract_sentinels(text: &str) -> (String, Vec<Sentinel>) {
    extract_sentinels_with(text, true)
}

/// `lead` gates the test_cases marker: it is an issue-level, lead-only
/// protocol, so on WORKER timelines the block stays in the body verbatim — a
/// worker quoting protocol text (or prompt-injected repo content) must neither
/// write the issue document nor have its quoted text silently vanish.
pub fn extract_sentinels_with(text: &str, lead: bool) -> (String, Vec<Sentinel>) {
    let mut out = String::with_capacity(text.len());
    let mut found = Vec::new();
    let mut rest = text;
    loop {
        // Earliest marker wins; the open tags are mutually non-prefixing so
        // positions never tie.
        let tc = if lead { rest.find(OPEN_TC) } else { None };
        let next = [
            (rest.find(OPEN_AC), Kind::ActionCard),
            (rest.find(OPEN_PC), Kind::PlanCard),
            (tc, Kind::TestCases),
            (rest.find(LIST_REPOS), Kind::ListRepos),
        ]
        .into_iter()
        .filter_map(|(pos, kind)| pos.map(|p| (p, kind)))
        .min_by_key(|(pos, _)| *pos);
        let Some((pos, kind)) = next else {
            out.push_str(rest);
            break;
        };
        let more = match kind {
            Kind::ListRepos => {
                out.push_str(&rest[..pos]);
                found.push(Sentinel::ListRepos);
                rest = &rest[pos + LIST_REPOS.len()..];
                true
            }
            Kind::ActionCard => consume_card(
                &mut out,
                &mut rest,
                pos,
                (OPEN_AC, CLOSE_AC),
                &mut found,
                Sentinel::ActionCard,
            ),
            Kind::PlanCard => consume_card(
                &mut out,
                &mut rest,
                pos,
                (OPEN_PC, CLOSE_PC),
                &mut found,
                Sentinel::PlanCard,
            ),
            Kind::TestCases => consume_card(
                &mut out,
                &mut rest,
                pos,
                (OPEN_TC, CLOSE_TC),
                &mut found,
                Sentinel::TestCases,
            ),
        };
        if !more {
            break;
        }
    }
    (out, found)
}

/// Consume one `open…close` card starting at `pos`, pushing the extracted
/// payload via `make`. Returns false when the card is unclosed — the caller
/// keeps the remainder verbatim and stops scanning.
fn consume_card<'a>(
    out: &mut String,
    rest: &mut &'a str,
    pos: usize,
    (open, close): (&str, &str),
    found: &mut Vec<Sentinel>,
    make: fn(String) -> Sentinel,
) -> bool {
    let after_open = pos + open.len();
    if let Some(close_rel) = rest[after_open..].find(close) {
        out.push_str(&rest[..pos]);
        found.push(make(rest[after_open..after_open + close_rel].to_string()));
        *rest = &rest[after_open + close_rel + close.len()..];
        true
    } else {
        // Unclosed — keep the rest as plain text so a half-typed sentinel
        // never eats the tail of the assistant message.
        out.push_str(rest);
        false
    }
}

/// Heal model-side over-escaping in a card's JSON payload before it is
/// persisted/rendered: leads sometimes write `\\n` inside the sentinel JSON
/// (one escape level too many), so the decoded string carries a LITERAL
/// backslash-n and the plan card renders "\n\n" as text. Per string value,
/// collapse literal `\n`/`\t` sequences to real whitespace ONLY when the
/// string contains no real newline at all — a correctly-escaped payload has
/// real newlines and is left untouched, which also protects legit
/// backslash-n prose (e.g. code snippets) inside multi-line strings.
/// Non-JSON payloads pass through unchanged.
pub fn normalize_card_json(json: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_string();
    };
    fn heal(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::String(s) => {
                if !s.contains('\n') && s.contains("\\n") {
                    *s = s.replace("\\n", "\n").replace("\\t", "\t");
                }
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(heal),
            serde_json::Value::Object(o) => o.values_mut().for_each(heal),
            _ => {}
        }
    }
    heal(&mut v);
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_over_escaped_newlines() {
        // The exact artifact seen live: `\\n\\n` between sections (decoded to a
        // literal backslash-n) with no real newline anywhere in the string.
        let raw = r#"{"approach":"**契约**：一段\\n\\n**交互**：另一段","tasks":[{"note":"a\\nb"}]}"#;
        let healed = normalize_card_json(raw);
        let v: serde_json::Value = serde_json::from_str(&healed).expect("valid json");
        assert_eq!(v["approach"].as_str(), Some("**契约**：一段\n\n**交互**：另一段"));
        assert_eq!(v["tasks"][0]["note"].as_str(), Some("a\nb"));
    }

    #[test]
    fn normalize_keeps_correct_payloads_and_mixed_strings() {
        // A properly-escaped payload (real newline present) keeps its literal
        // backslash-n untouched — e.g. code snippets explaining "\n".
        let raw = "{\"approach\":\"first line\\nuses \\\\n as separator\"}";
        let healed = normalize_card_json(raw);
        let v: serde_json::Value = serde_json::from_str(&healed).expect("valid json");
        assert_eq!(v["approach"].as_str(), Some("first line\nuses \\n as separator"));
        // Non-JSON passes through verbatim.
        assert_eq!(normalize_card_json("not json"), "not json");
    }
}
