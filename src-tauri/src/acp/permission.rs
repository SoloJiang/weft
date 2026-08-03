//! Map Weft allow/deny decisions onto ACP `session/request_permission` options.
//! Session-scoped always-cache only — no durable grants here.

use std::collections::HashMap;

use serde_json::Value;

/// Which permission option class we want to select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl Want {
    fn kind_str(self) -> &'static str {
        match self {
            Want::AllowOnce => "allow_once",
            Want::AllowAlways => "allow_always",
            Want::RejectOnce => "reject_once",
            Want::RejectAlways => "reject_always",
        }
    }

    pub fn is_allow(self) -> bool {
        matches!(self, Want::AllowOnce | Want::AllowAlways)
    }
}

/// Pick an `optionId` from the agent's offered options for `want`.
/// Prefers matching `kind`, then `optionId` string equality with the kind name.
pub fn pick_option_id(options: &[Value], want: Want) -> Option<String> {
    let kind = want.kind_str();
    for opt in options {
        if opt.get("kind").and_then(|k| k.as_str()) == Some(kind) {
            if let Some(id) = opt.get("optionId").and_then(|i| i.as_str()) {
                return Some(id.to_string());
            }
        }
    }
    for opt in options {
        if opt.get("optionId").and_then(|i| i.as_str()) == Some(kind) {
            return Some(kind.to_string());
        }
    }
    // Soft fallback: first allow_* / reject_* by prefix.
    let prefix = if want.is_allow() { "allow" } else { "reject" };
    for opt in options {
        let id = opt.get("optionId").and_then(|i| i.as_str()).unwrap_or("");
        let k = opt.get("kind").and_then(|i| i.as_str()).unwrap_or("");
        if id.starts_with(prefix) || k.starts_with(prefix) {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// Build the ACP permission result body for a selected optionId.
pub fn selected_outcome(option_id: &str) -> Value {
    serde_json::json!({
        "outcome": {
            "outcome": "selected",
            "optionId": option_id,
        }
    })
}

/// Stable cache key for always-allow / always-deny within one ACP session.
pub fn intent_key(tool_kind: &str, raw_input: &Value) -> String {
    if let Some(cmd) = raw_input.get("command").and_then(|c| c.as_str()) {
        // Coarse: tool family only for always (matches omp cacheKey: toolName).
        // Keep command out so "always allow bash" is session-wide for bash.
        let _ = cmd;
        return format!("{tool_kind}:bash");
    }
    if tool_kind == "edit" || tool_kind == "delete" || tool_kind == "move" {
        return format!("edit:{tool_kind}");
    }
    if let Some(k) = raw_input.get("kind").and_then(|k| k.as_str()) {
        return format!("{tool_kind}:{k}");
    }
    tool_kind.to_string()
}

/// What a permission request is actually asking to DO — one discriminated
/// value the risk classifier maps exhaustively.
///
/// Derived from the `toolCall` itself and never re-parsed out of
/// [`intent_key`]: that key is a deliberately lossy grouping token for
/// always-grants (`execute:bash`, `edit:delete`, or a bare `read`), so
/// comparing it against bare verbs silently fails. A live omp shell request
/// arrives as kind `execute` + `rawInput.command` and keys to `execute:bash`,
/// which matches neither `"bash"` nor `"execute"` — every command, `rm -rf`
/// included, was scored as a generic tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionIntent {
    /// A shell command line — the request carries `rawInput.command`.
    Command(String),
    /// A filesystem read. EVERY named location, because the risk of the whole
    /// request is the risk of its worst target — a read whose second path is
    /// an SSH key is not a read-only request. Empty when none were named.
    Read { paths: Vec<String> },
    /// A filesystem mutation: edit, write, delete, or move. Same "every
    /// location" rule as [`Self::Read`].
    Write { paths: Vec<String> },
    /// Network access ACP itself declared as such (tool kind `fetch`) — the
    /// same "the engine already identified it" case `RiskSignal::Network`
    /// exists for, so it needs no further scanning.
    Network,
    /// A GUI/computer-use action via omp's built-in `computer`/`browser`
    /// tool (see `is_gui_tool_call`'s doc for how this is detected, and the
    /// `available_commands_update` listing in
    /// `tests/fixtures/acp/omp-spike-transcript.json` for where those two
    /// tool names come from) — M2-B. Carries the action name so the risk
    /// mapping can defer to `ask::classify_gui_action`, the SAME word list
    /// `weft_computer`'s MCP path (`bus::server::summarize`) uses, so a
    /// `left_click` reads as the same tier from either route.
    Gui { action: String },
    /// A call to weft's OWN injected `weft_computer` MCP tool
    /// — recognized by the unambiguous
    /// server-qualified tool-name forms in the title (see
    /// [`is_weft_computer_mcp_call`]) and pulled out BEFORE the native-GUI
    /// classification, which would otherwise match the same
    /// `rawInput.action` shape and reject it. The request's actual side
    /// effect is an HTTP call to weft's own `bus::computer_srv` gate chain
    /// (enabled check, approval card, control lease, throttle, Stop, audit),
    /// so the ACP consumer auto-allows it rather than rejecting it as a
    /// native GUI action or double-carding what that server already cards —
    /// the exact posture `bus::server`'s `AUTO_APPROVED_INTERNAL_TOOLS`
    /// takes for this same tool on the claude/opencode hook path.
    WeftComputerMcp { action: String },
    /// Anything else; carries the ACP tool kind so the card can name it.
    Other { kind: String },
}

/// omp's own built-in GUI tools (`/computer`, `/browser` — see their entries
/// in the `available_commands_update` listing,
/// `tests/fixtures/acp/omp-spike-transcript.json`) reach the permission
/// layer as an ordinary `toolCall`, not routed through weft's
/// `weft_computer` MCP server (that path is `bus::server::summarize`'s job
/// — see its doc for the analogous, MCP-side recognition).
///
/// A native GUI toolCall is recognized two ways, EITHER sufficient:
///
///  1. its display `title` is the bare `computer`/`browser` string omp's
///     built-in tool carries today, OR
///  2. its structured `rawInput.action` names a RECOGNIZED GUI action (the
///     same closed word list `ask::classify_gui_action` tiers).
///
/// title alone is NOT an
/// authoritative, immutable tool identity — the only evidence that a native
/// GUI toolCall arrives titled exactly `computer`/`browser` is the
/// `available_commands_update` SLASH-COMMAND listing in
/// `tests/fixtures/acp/omp-spike-transcript.json`, which names TOGGLE commands,
/// not toolCall titles. An action-specific or localized title would otherwise
/// route a native GUI action down the ordinary-tool path, where a read-only or
/// dangerous-mode grant could auto-approve an OS operation omp performs itself,
/// outside weft's control lease / Escape guard / completion checks. Reading the
/// structured `rawInput.action` — the action the tool will ACTUALLY perform,
/// the same field [`gui_action`] already trusts — closes that: it is a
/// stronger identity signal than the title, and the word list is specific
/// enough that a non-GUI tool coincidentally carrying one of these exact action
/// verbs is only ever mislabeled MORE closed (its own card becomes a computer
/// card), never waved through.
///
/// ACP's own `kind` is a small, protocol-defined vocabulary
/// (`read`/`edit`/`delete`/`move`/`search`/`execute`/`fetch`/`think`/`other`)
/// with no `computer`/`browser` member, and no `toolCall` this file has ever
/// seen carries a `name` field — only `toolCallId`/`title`/`kind`/`rawInput`/
/// `content`/`locations` (see `tool_paths`/`summary_from_params`) — so these
/// two are the strongest signals available.
fn is_gui_tool_call(tc: &Value) -> bool {
    if matches!(
        tc.get("title").and_then(|t| t.as_str()),
        Some("computer") | Some("browser")
    ) {
        return true;
    }
    tc.get("rawInput")
        .and_then(|r| r.get("action"))
        .and_then(|a| a.as_str())
        .is_some_and(crate::ask::is_known_gui_action)
}

/// Whether this toolCall is weft's OWN injected `weft_computer` MCP tool
/// . the earlier broadening
/// of [`is_gui_tool_call`] (any `rawInput.action` naming a known GUI verb)
/// also matches the injected MCP tool's OWN argument shape — its `rawInput`
/// IS `{"action": "left_click", ...}` — so without this carve-out, an agent
/// that permission-gates its MCP calls (omp can) had every legitimate
/// `weft_computer` call rejected by the native-GUI gate before weft's own
/// server-side permission chain ever saw it.
///
/// Recognized STRICTLY by the two server-qualified tool-name forms this repo
/// can vouch for — claude-style `mcp__weft_computer__computer` (unambiguous
/// `__` delimiters) and the one vouched flattened literal
/// `weft_computer_computer` — mirroring `bus::server::split_internal_tool`'s
/// exact rule AND its documented limitation: an agent whose MCP tool naming
/// differs (unconfirmed for omp today, exactly like codex there) is NOT
/// matched, so its computer calls keep the fail-closed native rejection until
/// the real shape is confirmed on a live transcript and added here. That
/// asymmetry is deliberate: an unrecognized form breaks the feature LOUDLY (a
/// rejected call the agent reports), while a guessed form could quietly
/// wave through something that isn't the weft server at all.
///
/// Trust note: the title is authored by the agent RUNTIME describing its own
/// tool call (the model picks tools/arguments; it does not name them), which
/// is the same provenance the hook paths trust for their runtime-reported
/// `mcp__weft_computer__computer` tool names — and no agent runtime titles a
/// NATIVE built-in tool with another server's qualified MCP name. Allowing
/// is also bounded: approval releases whatever call the runtime actually
/// routed, and a genuinely MCP-routed call lands on weft's own gate chain.
fn is_weft_computer_mcp_call(tc: &Value) -> bool {
    matches!(
        tc.get("title").and_then(|t| t.as_str()),
        Some("mcp__weft_computer__computer") | Some("weft_computer_computer")
    )
}

/// The GUI/computer-use action name for a `computer`/`browser` toolCall: the
/// structured `rawInput.action` weft's own `weft_computer` MCP tool uses
/// (`screenshot`, `left_click`, …) when the backend sends it, else the ACP
/// `kind` as a fallback label — `classify_gui_action` (`ask.rs`) honestly
/// defaults an unrecognized action to `Unknown` rather than guessing, so a
/// fallback label that happens not to match its word list is safe, not a
/// silent misclassification.
fn gui_action(raw_input: &Value, kind: &str) -> String {
    raw_input
        .get("action")
        .and_then(|a| a.as_str())
        .unwrap_or(kind)
        .to_string()
}

/// Every path a file-shaped request names: ACP's own structured `locations`
/// first, then the raw-input key names agents actually use.
///
/// ALL of them, not just the first. The first path decided the risk tier once,
/// which meant a multi-file read could be tiered `ReadOnly` off an ordinary
/// leading path while a later entry touched a credential — and a read-only
/// grant would then auto-approve it unseen.
///
/// An empty result is not a downgrade: `classify_file` tiers on the tool verb,
/// so a write that named no location is still a write.
fn tool_paths(tc: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |out: &mut Vec<String>, p: &str| {
        if !p.is_empty() && !out.iter().any(|seen| seen == p) {
            out.push(p.to_string());
        }
    };
    for p in tc
        .get("locations")
        .and_then(|l| l.as_array())
        .into_iter()
        .flatten()
        .filter_map(|l| l.get("path").and_then(|p| p.as_str()))
    {
        push(&mut out, p);
    }
    // BOTH sources, not "structured first, raw as fallback". Treating them as
    // alternatives let a request put an ordinary file in `locations` and a
    // credential in `rawInput.path`: only the ordinary one was classified, the
    // request came out `ReadOnly`, and a read-only grant approved the
    // credential read with no card. Every raw key is collected for the same
    // reason — they can name different files.
    if let Some(raw) = tc.get("rawInput") {
        for key in ["path", "file_path", "filePath", "abs_path", "absPath"] {
            if let Some(p) = raw.get(key).and_then(|p| p.as_str()) {
                push(&mut out, p);
            }
        }
    }
    out
}

/// Classify a full permission request params object.
pub fn intent_from_params(params: &Value) -> PermissionIntent {
    let tc = params.get("toolCall").unwrap_or(&Value::Null);
    let kind = tc.get("kind").and_then(|k| k.as_str()).unwrap_or("tool");
    // A command line is a command whatever kind it is declared under — the
    // same precedence `intent_key` gives it, and an agent that sends a shell
    // line under kind `other` still runs a shell line.
    if let Some(cmd) = tc
        .get("rawInput")
        .and_then(|r| r.get("command"))
        .and_then(|c| c.as_str())
    {
        return PermissionIntent::Command(cmd.to_string());
    }
    // weft's own injected `weft_computer` MCP tool,
    // pulled out BEFORE the native-GUI branch below (its rawInput carries the
    // same `action` shape that branch matches — see
    // `is_weft_computer_mcp_call`'s doc). AFTER the command check above,
    // deliberately: a payload carrying BOTH a weft-computer title and a
    // `rawInput.command` classifies as the shell command it would run —
    // carded as one, never auto-allowed.
    if is_weft_computer_mcp_call(tc) {
        let raw = tc.get("rawInput").cloned().unwrap_or(Value::Null);
        return PermissionIntent::WeftComputerMcp {
            action: gui_action(&raw, kind),
        };
    }
    // omp's built-in computer/browser tool (M2-B, see `is_gui_tool_call`'s
    // doc): its rawInput carries an `action`, a shape none of the `kind`
    // branches below recognize, so — like the command check just above — it
    // needs to be pulled out ahead of that match rather than folded into it.
    if is_gui_tool_call(tc) {
        let raw = tc.get("rawInput").cloned().unwrap_or(Value::Null);
        return PermissionIntent::Gui {
            action: gui_action(&raw, kind),
        };
    }
    match kind {
        "read" | "search" => PermissionIntent::Read {
            paths: tool_paths(tc),
        },
        "edit" | "write" | "delete" | "move" => PermissionIntent::Write {
            paths: tool_paths(tc),
        },
        "fetch" => PermissionIntent::Network,
        other => PermissionIntent::Other {
            kind: other.to_string(),
        },
    }
}

/// The canonical, EXACT action identity for an Always grant.
///
/// The stringified `rawInput` alone is NOT it. ACP lets a request carry its
/// target only in `toolCall.locations` — which is exactly where
/// [`intent_from_params`] looks first — and two edits to different files then
/// stringify to an identical, sometimes empty, `rawInput`. An Always grant
/// keyed on that would let approving one file silently auto-approve another.
///
/// EVERY named location joins the key, not just the first that `tool_path`
/// takes for display and risk tiering: otherwise a two-file request could be
/// replayed under the same grant with its second file swapped out.
///
/// Never shown to a human — `summary`/`detail` are the display strings.
///
/// Each location is LENGTH-PREFIXED rather than joined by a separator: a path
/// may contain any byte except `/` and NUL, so any delimiter could in
/// principle be forged to make two different location sets serialize
/// identically. Lengths make the encoding unambiguous outright.
pub fn grant_identity(params: &Value) -> String {
    let tc = params.get("toolCall").unwrap_or(&Value::Null);
    // Every field that can carry the action, not just the two that usually do.
    // A sparse request may omit BOTH `rawInput` and `locations` and describe
    // itself in `title`/`content` alone; keyed on the other two, two different
    // `execute` requests collapsed to the same identity, and an Always granted
    // to the first silently approved the second.
    let mut out = String::new();
    for part in [
        tc.get("rawInput"),
        tc.get("locations"),
        tc.get("title"),
        tc.get("content"),
        tc.get("kind"),
    ] {
        // `to_string()` on a JSON value is stable for a given structure, and
        // absent vs. present-but-null stay distinguishable (`""` vs `"null"`).
        let encoded = part.map(|v| v.to_string()).unwrap_or_default();
        out.push_str(&format!("{}:{encoded}", encoded.len()));
    }
    out
}

/// Intent key from a full permission request params object.
pub fn intent_key_from_params(params: &Value) -> String {
    let tc = params.get("toolCall").unwrap_or(&Value::Null);
    let kind = tc
        .get("kind")
        .and_then(|k| k.as_str())
        .unwrap_or("tool");
    let raw = tc.get("rawInput").cloned().unwrap_or(Value::Null);
    // Mirrors `intent_from_params`'s carve-out, INCLUDING its
    // command-first precedence (the `command` guard here — a weft-titled
    // payload carrying `rawInput.command` classifies as the shell command it
    // would run, so it must key as one too). Defensive only: the ACP consumer
    // auto-allows this intent before any grant is ever recorded, so this key
    // never persists — but if that ever changes, a weft-MCP grant must not
    // share a bucket with a native `gui:*` one.
    if is_weft_computer_mcp_call(tc) && raw.get("command").and_then(|c| c.as_str()).is_none() {
        return format!("weft_computer:{}", gui_action(&raw, kind));
    }
    // Mirrors `intent_from_params`'s own Gui carve-out: an Always grant for a
    // GUI action is scoped to the ACTION (`gui:screenshot`, `gui:left_click`,
    // …), not lumped into one coarse `tool`/`other` bucket the way `intent_key`
    // groups everything else — a `left_click` "always allow" must not also
    // silently cover an unrelated `type` action.
    if is_gui_tool_call(tc) {
        return format!("gui:{}", gui_action(&raw, kind));
    }
    intent_key(kind, &raw)
}

/// Human summary line for the Needs-you card.
/// Readable text from a `toolCall.content` block list.
///
/// Deliberately NOT `map::extract_tool_output`: that one reads tool RESULTS,
/// so it prefers `rawOutput` and drops a leading `$ cmd` echo. Here that echo
/// is the single most useful line — it is what the human is being asked to
/// approve.
fn content_text(tc: &Value) -> String {
    let Some(arr) = tc.get("content").and_then(|c| c.as_array()) else {
        return String::new();
    };
    let mut parts: Vec<&str> = Vec::new();
    for item in arr {
        if let Some(t) = item.pointer("/content/text").and_then(|t| t.as_str()) {
            parts.push(t);
        } else if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
            parts.push(t);
        }
    }
    parts.join("\n")
}

pub fn summary_from_params(params: &Value) -> (String, String) {
    let tc = params.get("toolCall").unwrap_or(&Value::Null);
    // Absent title becomes EMPTY, not the literal "tool": the empty branch
    // below is what emits the machine token the catalogs localize, and a
    // non-empty placeholder made that branch unreachable — so a request
    // without a title showed an untranslated `tool` in every locale.
    let title = tc.get("title").and_then(|t| t.as_str()).unwrap_or_default();
    // `rawInput` alone is not enough to authorize by. An ACP request may name
    // its targets ONLY in `toolCall.locations` — the shape the intent and
    // grant-identity code reads first — leaving `rawInput` empty. With a
    // generic or empty title, the card would then ask a human to approve a
    // file operation without naming a single path. Append whatever locations
    // were named so the detail always says what will be touched.
    let raw = tc
        .get("rawInput")
        .map(|r| r.to_string())
        .unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    if !raw.is_empty() {
        parts.push(raw);
    }
    parts.extend(tool_paths(tc));
    // `content` is a LAST RESORT, not an addition. For an ordinary request it
    // would only restate `rawInput` in another form (`{"command":"ls"}` and
    // `$ ls`), so it is used exactly when the action is described nowhere else
    // — the sparse shape ACP permits and `grant_identity` already treats as
    // action-bearing. Without it that card body is EMPTY and the human is
    // asked to approve something unnamed, which leaves only blind-approve or
    // refuse: both product failures.
    let detail = if parts.is_empty() {
        content_text(tc)
    } else {
        parts.join("\n")
    };
    // M2-B: a GUI/computer-use toolCall gets its own summary format
    // (`computer: <action>`) rather than the bare "computer"/"browser"
    // title — naming the ACTION is what the human needs to triage at a
    // glance, the same reason `bus::server::summarize` does the same thing
    // for `weft_computer`'s MCP path. `detail` is unaffected — the generic
    // construction above already names whatever `rawInput`/`locations` this
    // toolCall carries.
    if is_gui_tool_call(tc) {
        let kind = tc.get("kind").and_then(|k| k.as_str()).unwrap_or("tool");
        let raw_value = tc.get("rawInput").cloned().unwrap_or(Value::Null);
        return (format!("computer: {}", gui_action(&raw_value, kind)), detail);
    }
    // Stable machine token — frontend i18n maps `acp.permission_required`.
    let summary = if title.is_empty() {
        "acp.permission_required".into()
    } else {
        title.to_string()
    };
    (summary, detail)
}

#[derive(Debug, Default, Clone)]
pub struct AlwaysCache {
    map: HashMap<String, Want>,
}

impl AlwaysCache {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<Want> {
        self.map.get(key).copied()
    }

    pub fn set(&mut self, key: String, want: Want) {
        if matches!(want, Want::AllowAlways | Want::RejectAlways) {
            self.map.insert(key, want);
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_options() -> Vec<Value> {
        vec![
            json!({"optionId":"allow_once","name":"Allow once","kind":"allow_once"}),
            json!({"optionId":"allow_always","name":"Always allow","kind":"allow_always"}),
            json!({"optionId":"reject_once","name":"Reject","kind":"reject_once"}),
            json!({"optionId":"reject_always","name":"Always reject","kind":"reject_always"}),
        ]
    }

    #[test]
    fn picks_by_kind() {
        let opts = sample_options();
        assert_eq!(
            pick_option_id(&opts, Want::AllowOnce).as_deref(),
            Some("allow_once")
        );
        assert_eq!(
            pick_option_id(&opts, Want::RejectAlways).as_deref(),
            Some("reject_always")
        );
    }

    #[test]
    fn selected_outcome_shape() {
        let v = selected_outcome("allow_once");
        assert_eq!(v["outcome"]["outcome"], "selected");
        assert_eq!(v["outcome"]["optionId"], "allow_once");
    }

    #[test]
    fn intent_key_bash_is_coarse() {
        let k = intent_key("execute", &json!({"command": "echo hi"}));
        assert_eq!(k, "execute:bash");
        let k2 = intent_key("execute", &json!({"command": "rm -rf /"}));
        assert_eq!(k, k2, "always-allow is session-wide for bash family");
    }

    #[test]
    fn always_cache_only_stores_always_variants() {
        let mut c = AlwaysCache::new();
        c.set("k".into(), Want::AllowOnce);
        assert!(c.get("k").is_none());
        c.set("k".into(), Want::AllowAlways);
        assert_eq!(c.get("k"), Some(Want::AllowAlways));
    }

    #[test]
    fn summary_from_permission_fixture_shape() {
        let params = json!({
            "sessionId": "s",
            "toolCall": {
                "toolCallId": "c1",
                "title": "echo TOOL_OK",
                "kind": "execute",
                "rawInput": { "command": "echo TOOL_OK" }
            }
        });
        let (s, _) = summary_from_params(&params);
        assert_eq!(s, "echo TOOL_OK");
        assert_eq!(intent_key_from_params(&params), "execute:bash");
    }

    /// Pinned to the live omp capture, not a hand-written shape: this is the
    /// exact frame a real shell approval arrives as. `intent_key` folds it to
    /// `execute:bash` for grant grouping — matching risk against THAT spelling
    /// is what scored every command, `rm -rf` included, as a generic tool call.
    #[test]
    fn live_omp_shell_request_classifies_as_a_command() {
        let raw = include_str!("../../tests/fixtures/acp/sample-permission-request.json");
        let frame: Value = serde_json::from_str(raw).expect("fixture parses");
        let params = frame.get("params").expect("fixture carries params");

        assert_eq!(
            intent_from_params(params),
            PermissionIntent::Command("echo TOOL_OK".into())
        );
        // The lossy grant key is deliberately unchanged — different purpose.
        assert_eq!(intent_key_from_params(params), "execute:bash");
    }

    #[test]
    fn file_intents_split_read_from_write_and_prefer_acp_locations() {
        let read = json!({
            "toolCall": {
                "kind": "read",
                "rawInput": { "path": "raw/input.rs" },
                "locations": [{ "path": "src/from_locations.rs" }]
            }
        });
        assert_eq!(
            intent_from_params(&read),
            PermissionIntent::Read {
                paths: vec!["src/from_locations.rs".into(), "raw/input.rs".into()]
            },
            "structured locations lead, but a raw-input path is still a target"
        );

        for kind in ["edit", "write", "delete", "move"] {
            let params = json!({
                "toolCall": { "kind": kind, "rawInput": { "file_path": "src/main.rs" } }
            });
            assert_eq!(
                intent_from_params(&params),
                PermissionIntent::Write {
                    paths: vec!["src/main.rs".into()]
                },
                "{kind} mutates the filesystem"
            );
        }
    }

    /// Every location is carried, in order. Taking only the first meant a
    /// multi-file read could be tiered off an ordinary leading path while a
    /// later entry was a credential — and a read-only grant auto-approves a
    /// `ReadOnly` ask without ever showing the human the card.
    #[test]
    fn every_named_location_reaches_the_classifier() {
        let params = json!({
            "toolCall": {
                "kind": "read",
                "locations": [{"path":"src/main.rs"}, {"path":"/home/u/.ssh/id_rsa"}]
            }
        });
        assert_eq!(
            intent_from_params(&params),
            PermissionIntent::Read {
                paths: vec!["src/main.rs".into(), "/home/u/.ssh/id_rsa".into()]
            }
        );
    }

    /// `locations` and `rawInput` are not alternatives. Classifying only the
    /// structured one let a request hide a credential in the other and inherit
    /// the ordinary file's `ReadOnly` tier, which a read-only grant releases
    /// without a card.
    #[test]
    fn paths_come_from_locations_and_raw_input_together() {
        let params = json!({
            "toolCall": {
                "kind": "read",
                "rawInput": { "path": "/home/u/.ssh/id_rsa" },
                "locations": [{ "path": "src/main.rs" }]
            }
        });
        assert_eq!(
            intent_from_params(&params),
            PermissionIntent::Read {
                paths: vec!["src/main.rs".into(), "/home/u/.ssh/id_rsa".into()]
            }
        );
    }

    /// A path named in both places is one target, not two.
    #[test]
    fn a_path_named_twice_is_not_duplicated() {
        let params = json!({
            "toolCall": {
                "kind": "read",
                "rawInput": { "path": "src/main.rs", "file_path": "src/main.rs" },
                "locations": [{ "path": "src/main.rs" }]
            }
        });
        assert_eq!(
            intent_from_params(&params),
            PermissionIntent::Read {
                paths: vec!["src/main.rs".into()]
            }
        );
    }

    /// The card must name what it is authorizing. With the target only in
    /// `locations`, `rawInput` is empty and the detail said nothing at all.
    #[test]
    fn detail_names_locations_even_when_raw_input_is_empty() {
        let params = json!({
            "toolCall": {
                "kind": "edit",
                "locations": [{"path":"src/a.rs"}, {"path":"src/b.rs"}]
            }
        });
        let (_, detail) = summary_from_params(&params);
        assert!(detail.contains("src/a.rs"), "detail: {detail:?}");
        assert!(detail.contains("src/b.rs"), "detail: {detail:?}");
    }

    /// A write with nowhere to point is still a write: `classify_file` tiers on
    /// the verb, so an empty path must not demote it to a generic tool call.
    #[test]
    fn a_write_without_a_path_stays_a_write() {
        let params = json!({ "toolCall": { "kind": "edit" } });
        assert_eq!(
            intent_from_params(&params),
            PermissionIntent::Write { paths: Vec::new() }
        );
    }

    #[test]
    fn fetch_is_network_and_unknown_kinds_carry_their_name() {
        assert_eq!(
            intent_from_params(&json!({ "toolCall": { "kind": "fetch" } })),
            PermissionIntent::Network
        );
        assert_eq!(
            intent_from_params(&json!({ "toolCall": { "kind": "think" } })),
            PermissionIntent::Other {
                kind: "think".into()
            }
        );
        assert_eq!(
            intent_from_params(&json!({})),
            PermissionIntent::Other {
                kind: "tool".into()
            }
        );
    }

    /// An Always grant is persisted and replayed, so its key must be the FULL
    /// action. `detail` (the stringified `rawInput`) is not: ACP lets a request
    /// name its target only in `locations` — the field `intent_from_params`
    /// reads first — and two edits to different files then share an identical,
    /// here empty, `rawInput`. Keyed on that, approving one file would silently
    /// auto-approve the other.
    #[test]
    fn grant_identity_separates_requests_that_differ_only_in_locations() {
        let a = json!({"toolCall":{"kind":"edit","locations":[{"path":"src/a.rs"}]}});
        let b = json!({"toolCall":{"kind":"edit","locations":[{"path":"src/b.rs"}]}});

        // The bug in one line: `rawInput` — which WAS the whole key material —
        // is identical for both. (`detail` now appends the locations too, so
        // the card names them; the grant key must not depend on that.)
        assert_eq!(
            a.pointer("/toolCall/rawInput"),
            b.pointer("/toolCall/rawInput")
        );
        assert_ne!(grant_identity(&a), grant_identity(&b));
        // An identical request still shares its grant; that is what Always is.
        assert_eq!(grant_identity(&a), grant_identity(&a));
    }

    /// Every location joins the key, not just the first one `tool_path` uses:
    /// a two-file request must not be replayable with one file swapped.
    #[test]
    fn grant_identity_covers_every_location_not_just_the_first() {
        let a = json!({"toolCall":{"locations":[{"path":"keep"},{"path":"b"}]}});
        let b = json!({"toolCall":{"locations":[{"path":"keep"},{"path":"c"}]}});
        assert_ne!(grant_identity(&a), grant_identity(&b));
    }

    /// The product failure this closes: a card whose body is empty leaves the
    /// human only "approve something unnamed" or "refuse", and a request may
    /// legitimately describe its action only in `content` — which this module
    /// already treats as action-bearing in `grant_identity`. The two halves of
    /// the authorization decision must agree on what the action IS.
    #[test]
    fn a_content_only_request_still_says_what_it_will_do() {
        let params = json!({
            "toolCall": {
                "kind": "execute",
                "content": [
                    {"type":"content","content":{"type":"text","text":"$ rm -rf build"}}
                ]
            }
        });
        let (summary, detail) = summary_from_params(&params);

        assert!(
            detail.contains("rm -rf build"),
            "the card must name the action, got {detail:?}"
        );
        // No title either: the summary must be the token the catalogs localize,
        // not a literal that every locale shows in English.
        assert_eq!(summary, "acp.permission_required");
    }

    /// `content` is a fallback, not an addition — an ordinary request must not
    /// gain a second line restating `rawInput` in another form.
    #[test]
    fn content_does_not_duplicate_an_ordinary_requests_detail() {
        let params = json!({
            "toolCall": {
                "kind": "execute",
                "title": "echo TOOL_OK",
                "rawInput": { "command": "echo TOOL_OK" },
                "content": [
                    {"type":"content","content":{"type":"text","text":"$ echo TOOL_OK"}}
                ]
            }
        });
        let (summary, detail) = summary_from_params(&params);

        assert_eq!(summary, "echo TOOL_OK");
        assert!(!detail.contains("$ echo"), "content restated rawInput: {detail:?}");
        assert!(detail.contains("echo TOOL_OK"));
    }

    /// A sparse request — no `rawInput`, no `locations` — must still be
    /// distinguishable. Keyed on only those two, every such request collapsed
    /// to one identity, so an Always granted to the first `execute` silently
    /// approved a different one.
    #[test]
    fn sparse_requests_do_not_collapse_to_one_identity() {
        let a = json!({"toolCall": {"kind": "execute", "title": "deploy staging"}});
        let b = json!({"toolCall": {"kind": "execute", "title": "deploy production"}});
        assert_ne!(grant_identity(&a), grant_identity(&b));

        // Content-only differences count too.
        let c = json!({"toolCall": {"kind": "execute", "content": [{"text": "one"}]}});
        let d = json!({"toolCall": {"kind": "execute", "content": [{"text": "two"}]}});
        assert_ne!(grant_identity(&c), grant_identity(&d));

        // Absent stays distinguishable from present-but-null.
        let absent = json!({"toolCall": {"kind": "execute"}});
        let null_title = json!({"toolCall": {"kind": "execute", "title": null}});
        assert_ne!(grant_identity(&absent), grant_identity(&null_title));

        // And identical requests still share their grant.
        assert_eq!(grant_identity(&a), grant_identity(&a));
    }

    /// Length prefixes, not delimiters: a path can contain any byte but `/` and
    /// NUL, so a separator-joined encoding could be forged into a collision.
    #[test]
    fn grant_identity_boundaries_cannot_be_forged_from_path_bytes() {
        for probe in ["a\u{1f}b", "a\u{1e}b", "a:b", "2:ab"] {
            let one = json!({"toolCall":{"locations":[{"path":probe}]}});
            let two = json!({"toolCall":{"locations":[{"path":"a"},{"path":"b"}]}});
            assert_ne!(
                grant_identity(&one),
                grant_identity(&two),
                "path {probe:?} must not collide with a two-location request"
            );
        }
    }

    /// Whatever kind an agent declares, a command line runs a shell.
    #[test]
    fn a_command_under_any_kind_is_still_a_command() {
        let params = json!({
            "toolCall": { "kind": "other", "rawInput": { "command": "rm -rf /tmp/x" } }
        });
        assert_eq!(
            intent_from_params(&params),
            PermissionIntent::Command("rm -rf /tmp/x".into())
        );
    }

    // ---- Gui intent (M2-B: omp's built-in computer/browser tool) ---------------

    /// `title` is this file's most reliable tool-identity signal for the two
    /// omp-built-in GUI tools: ACP's own `kind` vocabulary has no
    /// computer/browser member (see `is_gui_tool_call`'s doc), and their
    /// names are pinned to what the omp transcript's own
    /// `available_commands_update` listing calls them.
    #[test]
    fn computer_and_browser_titles_classify_as_gui_intent() {
        for title in ["computer", "browser"] {
            let params = json!({
                "toolCall": {
                    "title": title,
                    "kind": "other",
                    "rawInput": { "action": "screenshot", "window": "Safari" }
                }
            });
            assert_eq!(
                intent_from_params(&params),
                PermissionIntent::Gui {
                    action: "screenshot".into()
                },
                "{title:?} toolCall should classify as Gui"
            );
        }
    }

    /// A GUI toolCall whose `rawInput` carries no `action` falls back to the
    /// ACP `kind` as a label — never invented, and `classify_gui_action`
    /// (`ask.rs`) honestly defaults an unrecognized label to `Unknown`
    /// rather than silently mapping it to a tier.
    #[test]
    fn gui_action_falls_back_to_kind_when_raw_input_has_no_action() {
        let params = json!({
            "toolCall": { "title": "computer", "kind": "other" }
        });
        assert_eq!(
            intent_from_params(&params),
            PermissionIntent::Gui {
                action: "other".into()
            }
        );
    }

    /// A `command`-shaped rawInput still wins over a `computer`-titled tool
    /// call — the same "a command line is a command whatever kind it's
    /// declared under" precedence `intent_from_params` already gives
    /// `PermissionIntent::Command`.
    #[test]
    fn a_command_under_a_computer_titled_tool_call_is_still_a_command() {
        let params = json!({
            "toolCall": {
                "title": "computer",
                "kind": "execute",
                "rawInput": { "command": "echo hi" }
            }
        });
        assert_eq!(
            intent_from_params(&params),
            PermissionIntent::Command("echo hi".into())
        );
    }

    /// An exact-name check, not a substring/prefix one — a title that merely
    /// contains "computer" must not misfire.
    #[test]
    fn a_title_that_merely_contains_computer_does_not_classify_as_gui() {
        let params = json!({
            "toolCall": { "title": "computer_science_notes.md", "kind": "read" }
        });
        assert_eq!(
            intent_from_params(&params),
            PermissionIntent::Read { paths: Vec::new() }
        );
    }

    /// a native GUI toolCall
    /// whose display title is NOT the bare `computer`/`browser` string —
    /// action-specific, localized, or otherwise renamed — is STILL recognized
    /// as GUI from its structured `rawInput.action`, so it can never slip down
    /// the ordinary-tool path where a read-only or dangerous-mode grant might
    /// auto-approve an OS operation omp performs outside weft's control lease.
    #[test]
    fn a_gui_action_is_recognized_from_raw_input_even_with_a_non_standard_title() {
        let localized = json!({
            "toolCall": {
                "title": "Contrôler l'ordinateur",
                "kind": "other",
                "rawInput": { "action": "left_click", "window": "Safari" }
            }
        });
        assert_eq!(
            intent_from_params(&localized),
            PermissionIntent::Gui {
                action: "left_click".into()
            },
            "a recognized GUI action must classify as Gui regardless of the display title"
        );
        // Its Always-grant key is action-scoped exactly like the title-matched path.
        assert_eq!(intent_key_from_params(&localized), "gui:left_click");

        // An UNRECOGNIZED action under an arbitrary title is NOT swept in — it
        // still classifies by its ACP `kind`, so the broadening only ever fails
        // MORE closed, never labels an ordinary tool GUI on a whim.
        let ordinary = json!({
            "toolCall": {
                "title": "do_thing",
                "kind": "read",
                "rawInput": { "action": "frobnicate" }
            }
        });
        assert_eq!(
            intent_from_params(&ordinary),
            PermissionIntent::Read { paths: Vec::new() },
            "an unrecognized action must not be misclassified as GUI"
        );
    }

    /// a permission request
    /// for weft's OWN injected `weft_computer` MCP tool — recognized by its
    /// server-qualified tool-name title — is NOT a native GUI intent, even
    /// though its `rawInput` carries the exact `action` shape the earlier
    /// broadening matches. It classifies as `WeftComputerMcp` (auto-allowed by
    /// the ACP consumer; the server-side gate chain owns the real approval)
    /// instead of being rejected before weft's own server ever sees the call.
    #[test]
    fn a_weft_computer_mcp_titled_call_classifies_as_mcp_not_gui() {
        for title in ["mcp__weft_computer__computer", "weft_computer_computer"] {
            let params = json!({
                "toolCall": {
                    "title": title,
                    "kind": "other",
                    "rawInput": { "action": "left_click", "window": "notes", "coordinate": [1, 2] }
                }
            });
            assert_eq!(
                intent_from_params(&params),
                PermissionIntent::WeftComputerMcp { action: "left_click".into() },
                "{title:?} must classify as the injected MCP tool, not native GUI"
            );
            assert_eq!(
                intent_key_from_params(&params),
                "weft_computer:left_click",
                "its (defensive) key must never share the native gui:* bucket"
            );
        }
    }

    /// the earlier carve-out keeps `intent_from_params`'s command-first
    /// precedence: a weft-computer-titled payload smuggling a
    /// `rawInput.command` classifies (and keys) as the shell command it would
    /// run — carded as one, never auto-allowed as an MCP computer call.
    #[test]
    fn a_weft_computer_title_with_a_command_is_still_a_command() {
        let params = json!({
            "toolCall": {
                "title": "mcp__weft_computer__computer",
                "kind": "execute",
                "rawInput": { "command": "rm -rf /" }
            }
        });
        assert_eq!(
            intent_from_params(&params),
            PermissionIntent::Command("rm -rf /".into())
        );
        assert_eq!(
            intent_key_from_params(&params),
            intent_key("execute", &json!({ "command": "rm -rf /" })),
            "the key must follow the command precedence too, not the weft_computer carve-out"
        );
    }

    /// Exact-name recognition only — near-miss titles (another server whose
    /// name embeds `weft_computer`, or a bare `weft_computer`) keep the
    /// fail-closed native-GUI rejection their `rawInput.action` earns them.
    #[test]
    fn a_near_miss_weft_computer_title_stays_native_gui() {
        for title in ["weft_computer", "mcp__weft_computer2__computer", "my_weft_computer_computer"] {
            let params = json!({
                "toolCall": {
                    "title": title,
                    "kind": "other",
                    "rawInput": { "action": "left_click" }
                }
            });
            assert_eq!(
                intent_from_params(&params),
                PermissionIntent::Gui { action: "left_click".into() },
                "{title:?} must NOT be recognized as the injected weft_computer tool"
            );
        }
    }

    /// Always-grant is scoped to the ACTION for a GUI intent, not lumped
    /// into one coarse bucket the way `intent_key` groups everything else —
    /// approving `left_click` forever must not also silently cover `type`.
    #[test]
    fn gui_intent_key_is_scoped_to_the_action() {
        let click = json!({
            "toolCall": { "title": "computer", "rawInput": { "action": "left_click" } }
        });
        let type_action = json!({
            "toolCall": { "title": "computer", "rawInput": { "action": "type" } }
        });
        assert_eq!(intent_key_from_params(&click), "gui:left_click");
        assert_eq!(intent_key_from_params(&type_action), "gui:type");
        assert_ne!(
            intent_key_from_params(&click),
            intent_key_from_params(&type_action),
            "an Always grant for left_click must not also cover type"
        );
    }

    #[test]
    fn gui_summary_names_the_action_not_the_bare_tool_title() {
        let params = json!({
            "toolCall": {
                "title": "computer",
                "kind": "other",
                "rawInput": { "action": "left_click", "window": "Safari" }
            }
        });
        let (summary, _detail) = summary_from_params(&params);
        assert_eq!(summary, "computer: left_click");
    }

    /// The real risk verdict (`lead_chat::engine::acp_permission_risk`)
    /// defers every `Gui` intent to `ask::classify_gui_action` — this pins
    /// that an observe-only action reads `ReadOnly` and an input-injecting
    /// one reads `Write`, the SAME word list `bus::server::summarize`'s
    /// `weft_computer` branch uses, so the two routes never disagree.
    #[test]
    fn gui_action_risk_mapping_matches_classify_gui_action() {
        let screenshot = json!({
            "toolCall": { "title": "computer", "rawInput": { "action": "screenshot" } }
        });
        let click = json!({
            "toolCall": { "title": "computer", "rawInput": { "action": "left_click" } }
        });
        match intent_from_params(&screenshot) {
            PermissionIntent::Gui { action } => assert_eq!(
                crate::ask::classify_gui_action(&action),
                crate::ask::RiskLevel::ReadOnly
            ),
            other => panic!("expected Gui intent, got {other:?}"),
        }
        match intent_from_params(&click) {
            PermissionIntent::Gui { action } => assert_eq!(
                crate::ask::classify_gui_action(&action),
                crate::ask::RiskLevel::Write
            ),
            other => panic!("expected Gui intent, got {other:?}"),
        }
    }
}
