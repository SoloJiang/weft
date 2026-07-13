//! lead_chat::sentinels::extract_sentinels — pulls `<weft:action_card>{...}`,
//! `<weft:plan_card>{...}` and `<weft:list_repos/>` markers out of assistant
//! text so the engine can persist card rows and answer list_repos via stdin
//! separately.
use weft::lead_chat::sentinels::{extract_sentinels, Sentinel};

#[test]
fn extracts_action_card() {
    let t = "前置 <weft:action_card>{\"title\":\"T\",\"actions\":[]}</weft:action_card> 后续";
    let (clean, found) = extract_sentinels(t);
    assert_eq!(clean.trim(), "前置  后续");
    assert_eq!(found.len(), 1);
    match &found[0] {
        Sentinel::ActionCard(json) => assert!(json.contains("\"title\":\"T\"")),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn extracts_plan_card() {
    let t = "讨论后 <weft:plan_card>{\"title\":\"P\",\"requirements\":[]}</weft:plan_card> 等确认";
    let (clean, found) = extract_sentinels(t);
    assert_eq!(clean.trim(), "讨论后  等确认");
    assert_eq!(found.len(), 1);
    match &found[0] {
        Sentinel::PlanCard(json) => assert!(json.contains("\"title\":\"P\"")),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn skips_malformed_plan_card_unclosed() {
    // 没有 closing tag，整段保持原样、不当作 sentinel
    let t = "before <weft:plan_card>{\"a\":1} no close";
    let (clean, found) = extract_sentinels(t);
    assert_eq!(clean, t);
    assert!(found.is_empty());
}

#[test]
fn plan_card_mixes_with_action_card_in_order() {
    let t = "<weft:plan_card>{\"p\":1}</weft:plan_card>x<weft:action_card>{\"a\":1}</weft:action_card>";
    let (clean, found) = extract_sentinels(t);
    assert_eq!(clean, "x");
    assert_eq!(found.len(), 2);
    assert!(matches!(found[0], Sentinel::PlanCard(_)));
    assert!(matches!(found[1], Sentinel::ActionCard(_)));
}

#[test]
fn extracts_list_repos() {
    let t = "before <weft:list_repos/> after";
    let (clean, found) = extract_sentinels(t);
    assert_eq!(clean.trim(), "before  after");
    assert!(matches!(found[0], Sentinel::ListRepos));
}

#[test]
fn ignores_unknown() {
    let t = "no sentinels <foo:bar/>";
    let (clean, found) = extract_sentinels(t);
    assert_eq!(clean, t);
    assert!(found.is_empty());
}

#[test]
fn extracts_multiple_mixed() {
    let t = "<weft:action_card>{\"a\":1}</weft:action_card><weft:list_repos/>";
    let (clean, found) = extract_sentinels(t);
    assert_eq!(clean.trim(), "");
    assert_eq!(found.len(), 2);
    assert!(matches!(found[0], Sentinel::ActionCard(_)));
    assert!(matches!(found[1], Sentinel::ListRepos));
}

#[test]
fn handles_no_sentinel() {
    let t = "plain assistant message";
    let (clean, found) = extract_sentinels(t);
    assert_eq!(clean, t);
    assert!(found.is_empty());
}

#[test]
fn skips_malformed_action_card_unclosed() {
    // 没有 closing tag，整段保持原样、不当作 sentinel
    let t = "before <weft:action_card>{\"a\":1} no close";
    let (clean, found) = extract_sentinels(t);
    assert_eq!(clean, t);
    assert!(found.is_empty());
}

#[test]
fn extracts_two_list_repos_in_row() {
    let t = "<weft:list_repos/><weft:list_repos/>";
    let (clean, found) = extract_sentinels(t);
    assert_eq!(clean, "");
    assert_eq!(found.len(), 2);
    assert!(matches!(found[0], Sentinel::ListRepos));
    assert!(matches!(found[1], Sentinel::ListRepos));
}

#[test]
fn action_card_json_can_contain_lt() {
    let t = r#"<weft:action_card>{"title":"a<b","actions":[]}</weft:action_card>"#;
    let (clean, found) = extract_sentinels(t);
    assert_eq!(clean, "");
    assert_eq!(found.len(), 1);
    match &found[0] {
        Sentinel::ActionCard(j) => assert!(j.contains("a<b")),
        _ => panic!(),
    }
}

#[test]
fn sentinel_butting_text_no_whitespace() {
    let t = "hello<weft:list_repos/>world";
    let (clean, found) = extract_sentinels(t);
    assert_eq!(clean, "helloworld");
    assert_eq!(found.len(), 1);
}
