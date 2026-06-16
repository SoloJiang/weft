//! Cross-repo dependency graph types (ARCHITECTURE §4.9). The curator is now a
//! pure agent: a read-only coding agent reads each repo deeply, classifies its
//! tier (frontend / gateway / backend), summarizes it, and reports the cross-repo
//! relations the agent sees. This module holds the shared data types and the
//! relation-merge logic; there is no deterministic manifest engine anymore (the
//! pipeline is agent-only — offline is not a supported mode).

/// The three architectural tiers a repo (or a monorepo sub-component) can fall
/// into. `gateway` is the middle layer — API gateways, BFFs, aggregators, edge
/// services, reverse proxies. The empty string means "not classified yet"
/// (analysis pending) and is rendered as an "analyzing" placeholder.
pub const TIERS: [&str; 3] = ["frontend", "gateway", "backend"];

/// Lowercase + validate a tier against `TIERS`. `None` for anything the agent
/// returns that isn't one of the three canonical tiers (caller stores "").
pub fn normalize_tier(tier: &str) -> Option<String> {
    let t = tier.trim().to_ascii_lowercase();
    TIERS.contains(&t.as_str()).then_some(t)
}

/// A directed dependency edge: `from` consumes `to`, evidenced by `via`.
///
/// `kind`/`source`/`confidence` are `serde(default)` so payloads written before
/// they existed still deserialize. Every edge now comes from the agent
/// (`source="agent"`, or `"user"` for a human-pinned relation).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Edge {
    pub from: i32,
    pub to: i32,
    pub via: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub confidence: u8,
}

/// The cross-repo relation kinds the curator may assert. Inferred kinds are
/// normalized to lowercase and dropped if not in this set, so the stored graph
/// stays consistent — a stray `HTTP` can't render unremovably. `lib` is a
/// declared package dependency (the agent reports these too now that there is no
/// manifest floor); the others are runtime / infra links.
pub const RELATION_KINDS: [&str; 5] = ["http", "grpc", "queue", "infra", "lib"];

/// Lowercase + validate a relation kind against `RELATION_KINDS`. None if it
/// isn't a recognized kind (caller drops it).
pub fn normalize_relation_kind(kind: &str) -> Option<String> {
    let k = kind.trim().to_ascii_lowercase();
    RELATION_KINDS.contains(&k.as_str()).then_some(k)
}

/// One agent-inferred outgoing relation: this repo depends on workspace repo
/// `to` via `kind` (http/grpc/queue/infra/lib), evidenced by `via`. `to` is a
/// repo_ref id the agent picks from the provided workspace list, so no fuzzy
/// resolution is needed. Persisted as a JSON array on the producer's profile.
#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct AgentRelation {
    pub to: i32,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub via: String,
    #[serde(default)]
    pub confidence: u8,
    /// "agent" (inferred) | "user" (human calibration — pinned). Empty == agent.
    #[serde(default)]
    pub source: String,
    /// A human-removed edge: kept as a tombstone so the auto pass won't
    /// resurrect it, and emits no graph edge.
    #[serde(default)]
    pub rejected: bool,
}

/// One sub-component of a monorepo, surfaced by the per-repo deep agent pass so
/// the repo map can offer an "expanded" view (the repo stays one node in the
/// overview, but expands to its internal packages/services). `deps` are the
/// names of SIBLING components in the same repo this one depends on, so the
/// expanded view can draw intra-repo edges without another agent round.
#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Component {
    /// `serde(default)` so one component object missing `name` doesn't make the
    /// whole per-repo classification unparseable — the caller drops nameless ones.
    #[serde(default)]
    pub name: String,
    /// Path relative to the repo root (e.g. "packages/api", "apps/web").
    #[serde(default)]
    pub path: String,
    /// frontend | gateway | backend | "" (unclassified).
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub summary: String,
    /// Names of sibling components (within the same repo) this one depends on.
    #[serde(default)]
    pub deps: Vec<String>,
}

/// Turn one repo's stored agent relations into edges, dropping self-links, any
/// `to` that is not a current workspace node (a stale id from a repo since
/// removed), and `rejected` tombstones. Empty `kind` falls back to "dep"; each
/// edge carries its relation's `source` (empty treated as "agent").
pub fn agent_edges(
    from_id: i32,
    relations: &[AgentRelation],
    nodes: &std::collections::HashSet<i32>,
) -> Vec<Edge> {
    relations
        .iter()
        .filter(|r| !r.rejected && r.to != from_id && nodes.contains(&r.to))
        .map(|r| Edge {
            from: from_id,
            to: r.to,
            via: r.via.clone(),
            kind: if r.kind.is_empty() { "dep".into() } else { r.kind.clone() },
            source: if r.source.is_empty() { "agent".into() } else { r.source.clone() },
            confidence: r.confidence,
        })
        .collect()
}

/// Merge a repo's current relations with a fresh agent pass: keep every
/// user-sourced relation (including `rejected` tombstones), drop old agent
/// relations, and add fresh agent relations EXCEPT any a user tombstone rejects
/// for the same (to, kind) — so a human-removed edge is never resurrected.
pub fn merge_relations(existing: &[AgentRelation], fresh_agent: &[AgentRelation]) -> Vec<AgentRelation> {
    let mut out: Vec<AgentRelation> =
        existing.iter().filter(|r| r.source == "user").cloned().collect();
    let tombstoned: std::collections::HashSet<(i32, String)> = out
        .iter()
        .filter(|r| r.rejected)
        .map(|r| (r.to, r.kind.clone()))
        .collect();
    for r in fresh_agent {
        if !tombstoned.contains(&(r.to, r.kind.clone())) {
            out.push(r.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn normalize_tier_accepts_canonical_only() {
        assert_eq!(super::normalize_tier("Frontend"), Some("frontend".into()));
        assert_eq!(super::normalize_tier(" GATEWAY "), Some("gateway".into()));
        assert_eq!(super::normalize_tier("backend"), Some("backend".into()));
        assert_eq!(super::normalize_tier("service"), None);
        assert_eq!(super::normalize_tier(""), None);
    }

    #[test]
    fn normalize_relation_kind_accepts_canonical_only() {
        assert_eq!(super::normalize_relation_kind("HTTP"), Some("http".into()));
        assert_eq!(super::normalize_relation_kind("Lib"), Some("lib".into()));
        assert_eq!(super::normalize_relation_kind("websocket"), None);
    }

    #[test]
    fn edge_deserializes_legacy_three_field_json() {
        // Payloads written before kind/source/confidence existed still parse, so
        // an upgraded build reads pre-existing data without a migration.
        let e: super::Edge = serde_json::from_str(r#"{"from":1,"to":2,"via":"x"}"#).unwrap();
        assert_eq!((e.from, e.to, e.via.as_str()), (1, 2, "x"));
        assert_eq!(e.kind, "");
        assert_eq!(e.source, "");
        assert_eq!(e.confidence, 0);
    }

    fn rel(to: i32, kind: &str, via: &str, confidence: u8) -> super::AgentRelation {
        super::AgentRelation {
            to,
            kind: kind.into(),
            via: via.into(),
            confidence,
            ..Default::default()
        }
    }

    #[test]
    fn agent_edges_drop_self_and_stale_targets() {
        let nodes: std::collections::HashSet<i32> = [1, 2, 3].into_iter().collect();
        let relations = vec![
            rel(2, "http", "GET /orders", 80), // kept
            rel(1, "http", "self call", 90),   // dropped: self-edge
            rel(9, "grpc", "Pricing.Quote", 70), // dropped: 9 not a node
        ];
        let edges = super::agent_edges(1, &relations, &nodes);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from, 1);
        assert_eq!(edges[0].to, 2);
        assert_eq!(edges[0].kind, "http");
        assert_eq!(edges[0].source, "agent");
        assert_eq!(edges[0].confidence, 80);
    }

    #[test]
    fn agent_edges_skip_rejected_and_carry_source() {
        let nodes: std::collections::HashSet<i32> = [1, 2].into_iter().collect();
        let relations = vec![
            super::AgentRelation {
                to: 2,
                kind: "http".into(),
                via: "GET /x".into(),
                confidence: 80,
                source: "user".into(),
                rejected: false,
            },
            super::AgentRelation {
                to: 2,
                kind: "grpc".into(),
                via: "Q".into(),
                confidence: 50,
                source: "user".into(),
                rejected: true, // a human-removed edge → no graph edge
            },
        ];
        let edges = super::agent_edges(1, &relations, &nodes);
        assert_eq!(edges.len(), 1, "rejected relation produces no edge");
        assert_eq!(edges[0].source, "user", "user-confirmed source flows to the edge");
        assert_eq!(edges[0].kind, "http");
    }

    #[test]
    fn merge_relations_keeps_user_replaces_agent_honors_tombstone() {
        let existing = vec![
            super::AgentRelation { to: 2, kind: "http".into(), via: "POST /pay".into(), confidence: 90, source: "user".into(), rejected: false },
            super::AgentRelation { to: 3, kind: "http".into(), via: "old".into(), confidence: 40, source: "user".into(), rejected: true },
            super::AgentRelation { to: 4, kind: "lib".into(), via: "stale-agent".into(), confidence: 100, source: "agent".into(), rejected: false },
        ];
        let fresh_agent = vec![
            super::AgentRelation { to: 3, kind: "http".into(), via: "re-found".into(), confidence: 70, source: "agent".into(), rejected: false },
            super::AgentRelation { to: 5, kind: "grpc".into(), via: "new".into(), confidence: 60, source: "agent".into(), rejected: false },
        ];
        let merged = super::merge_relations(&existing, &fresh_agent);
        assert!(merged.iter().any(|r| r.to == 2 && r.source == "user" && !r.rejected), "user edge survives");
        assert!(merged.iter().any(|r| r.to == 3 && r.rejected), "tombstone survives");
        assert!(!merged.iter().any(|r| r.to == 3 && !r.rejected), "tombstoned edge not resurrected");
        assert!(!merged.iter().any(|r| r.to == 4), "stale agent edge dropped");
        assert!(merged.iter().any(|r| r.to == 5 && r.source == "agent"), "new agent edge added");
    }

    #[test]
    fn component_deserializes_partial_json() {
        let c: super::Component =
            serde_json::from_str(r#"{"name":"web","tier":"frontend"}"#).unwrap();
        assert_eq!(c.name, "web");
        assert_eq!(c.tier, "frontend");
        assert_eq!(c.path, "");
        assert!(c.deps.is_empty());
    }
}
