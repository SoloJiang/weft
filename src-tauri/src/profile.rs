//! Cross-repo dependency graph types (ARCHITECTURE §4.9). The curator is a
//! hybrid pipeline: deterministic manifest edges (source="manifest") provide a
//! high-confidence floor, and the read-only agent fills in the runtime/infra
//! relations on top. Precedence: user > manifest > agent. This module holds the
//! shared data types and the relation-merge logic.

/// The architectural tiers a repo (or a monorepo sub-component) can fall into.
/// `backend` covers everything server-side (gateways/BFFs/aggregators included).
/// The empty string means "not classified yet" (analysis pending), rendered as
/// an "analyzing" placeholder.
pub const TIERS: [&str; 2] = ["frontend", "backend"];

/// Lowercase + validate a tier against `TIERS`. `None` for anything the agent
/// returns that isn't one of the two canonical tiers (caller stores "").
pub fn normalize_tier(tier: &str) -> Option<String> {
    let t = tier.trim().to_ascii_lowercase();
    TIERS.contains(&t.as_str()).then_some(t)
}

/// A directed dependency edge: `from` consumes `to`, evidenced by `via`.
///
/// `kind`/`source`/`confidence`/`rationale` are `serde(default)` so payloads written before
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
    /// Free-text rationale explaining why this dependency exists (agent-supplied).
    #[serde(default)]
    pub rationale: String,
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
    /// "agent" (inferred) | "manifest" (deterministic, from on-disk manifests) |
    /// "user" (human calibration — pinned). Empty == agent.
    #[serde(default)]
    pub source: String,
    /// A human-removed edge: kept as a tombstone so the auto pass won't
    /// resurrect it, and emits no graph edge.
    #[serde(default)]
    pub rejected: bool,
    /// Free-text rationale explaining why this dependency exists (agent-supplied).
    #[serde(default)]
    pub rationale: String,
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
    /// frontend | backend | "" (unclassified).
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub summary: String,
    /// Names of sibling components (within the same repo) this one depends on.
    #[serde(default)]
    pub deps: Vec<String>,
    /// Feature domains owned by this component (agent-assigned).
    #[serde(default)]
    pub domains: Vec<String>,
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
            rationale: r.rationale.clone(),
        })
        .collect()
}

/// Merge a repo's current relations with fresh manifest and agent passes.
///
/// Precedence: **user > manifest > agent**. The result is:
/// - All `user` relations (incl. `rejected` tombstones) — always kept.
/// - Fresh `manifest` relations, unless a user tombstone suppresses that
///   `(to, kind)` / `(to, kind, via)`.
/// - Fresh `agent` relations, unless suppressed by a user OR manifest relation
///   for that `(to, kind[, via])`.
///
/// Suppression scope: a user/manifest relation with an empty `via` suppresses
/// the entire `(to, kind)` from the lower tier; one with a specific `via`
/// suppresses only that exact `(to, kind, via)`.
pub fn merge_relations(
    existing: &[AgentRelation],
    fresh_manifest: &[AgentRelation],
    fresh_agent: &[AgentRelation],
) -> Vec<AgentRelation> {
    use std::collections::HashSet;

    // Start with every user relation (pins + tombstones).
    let mut out: Vec<AgentRelation> =
        existing.iter().filter(|r| r.source == "user").cloned().collect();

    // Build user-owned suppression sets from the user slice already in `out`.
    let user_kind: HashSet<(i32, String)> = out
        .iter()
        .filter(|r| r.via.is_empty())
        .map(|r| (r.to, r.kind.clone()))
        .collect();
    let user_exact: HashSet<(i32, String, String)> = out
        .iter()
        .filter(|r| !r.via.is_empty())
        .map(|r| (r.to, r.kind.clone(), r.via.clone()))
        .collect();

    // Add fresh manifest relations not suppressed by a user claim.
    for r in fresh_manifest {
        if user_kind.contains(&(r.to, r.kind.clone()))
            || user_exact.contains(&(r.to, r.kind.clone(), r.via.clone()))
        {
            continue;
        }
        out.push(r.clone());
    }

    // Build manifest-owned suppression set: any manifest edge for (to, kind)
    // suppresses that entire (to, kind) from the agent tier — manifest is
    // deterministic, so a single manifest lib edge to repo X means the agent's
    // lib claim for the same (to, kind) adds no information.
    let manifest_kind: HashSet<(i32, String)> = out
        .iter()
        .filter(|r| r.source == "manifest")
        .map(|r| (r.to, r.kind.clone()))
        .collect();

    // Add fresh agent relations not suppressed by a user OR manifest claim.
    for r in fresh_agent {
        if user_kind.contains(&(r.to, r.kind.clone()))
            || user_exact.contains(&(r.to, r.kind.clone(), r.via.clone()))
            || manifest_kind.contains(&(r.to, r.kind.clone()))
        {
            continue;
        }
        out.push(r.clone());
    }

    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn normalize_tier_accepts_canonical_only() {
        assert_eq!(super::normalize_tier("Frontend"), Some("frontend".into()));
        assert_eq!(super::normalize_tier("backend"), Some("backend".into()));
        // "gateway" is no longer a canonical tier (folded into "backend").
        assert_eq!(super::normalize_tier("gateway"), None);
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
                ..Default::default()
            },
            super::AgentRelation {
                to: 2,
                kind: "grpc".into(),
                via: "Q".into(),
                confidence: 50,
                source: "user".into(),
                rejected: true, // a human-removed edge → no graph edge
                ..Default::default()
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
            super::AgentRelation { to: 2, kind: "http".into(), via: "POST /pay".into(), confidence: 90, source: "user".into(), rejected: false, ..Default::default() },
            // A via-scoped tombstone: the user removed exactly the "GET /orders" edge.
            super::AgentRelation { to: 3, kind: "http".into(), via: "GET /orders".into(), confidence: 40, source: "user".into(), rejected: true, ..Default::default() },
            super::AgentRelation { to: 4, kind: "lib".into(), via: "stale-agent".into(), confidence: 100, source: "agent".into(), rejected: false, ..Default::default() },
        ];
        let fresh_agent = vec![
            // Re-found the exact removed edge → suppressed by the tombstone.
            super::AgentRelation { to: 3, kind: "http".into(), via: "GET /orders".into(), confidence: 70, source: "agent".into(), rejected: false, ..Default::default() },
            // A DISTINCT edge to the same repo (different via) → NOT hidden by the tombstone.
            super::AgentRelation { to: 3, kind: "http".into(), via: "POST /payments".into(), confidence: 70, source: "agent".into(), rejected: false, ..Default::default() },
            super::AgentRelation { to: 5, kind: "grpc".into(), via: "new".into(), confidence: 60, source: "agent".into(), rejected: false, ..Default::default() },
            // EXACT duplicate of the user pin (same to/kind/via) → suppressed.
            super::AgentRelation { to: 2, kind: "http".into(), via: "POST /pay".into(), confidence: 80, source: "agent".into(), rejected: false, ..Default::default() },
            // DISTINCT relationship to the pinned repo (different via) → kept.
            super::AgentRelation { to: 2, kind: "http".into(), via: "GET /orders".into(), confidence: 80, source: "agent".into(), rejected: false, ..Default::default() },
        ];
        let merged = super::merge_relations(&existing, &[], &fresh_agent);
        assert!(merged.iter().any(|r| r.to == 2 && r.source == "user" && r.via == "POST /pay"), "user edge survives");
        assert_eq!(
            merged.iter().filter(|r| r.to == 2 && r.kind == "http" && r.via == "POST /pay").count(),
            1,
            "exact-duplicate agent edge is not stored twice",
        );
        assert!(
            merged.iter().any(|r| r.to == 2 && r.kind == "http" && r.via == "GET /orders" && r.source == "agent"),
            "a distinct agent edge to the same repo survives next to a user pin",
        );
        assert!(merged.iter().any(|r| r.to == 3 && r.rejected), "tombstone survives");
        assert!(
            !merged.iter().any(|r| r.to == 3 && !r.rejected && r.via == "GET /orders"),
            "the exact tombstoned edge is not resurrected",
        );
        assert!(
            merged.iter().any(|r| r.to == 3 && r.via == "POST /payments" && r.source == "agent"),
            "a distinct edge of the same kind is not hidden by a via-scoped tombstone",
        );
        assert!(!merged.iter().any(|r| r.to == 4), "stale agent edge dropped");
        assert!(merged.iter().any(|r| r.to == 5 && r.source == "agent"), "new agent edge added");
    }

    fn manifest_rel(to: i32, kind: &str, via: &str) -> super::AgentRelation {
        super::AgentRelation {
            to,
            kind: kind.into(),
            via: via.into(),
            confidence: 100,
            source: "manifest".into(),
            rejected: false,
            ..Default::default()
        }
    }

    fn user_tombstone(to: i32, kind: &str) -> super::AgentRelation {
        super::AgentRelation {
            to,
            kind: kind.into(),
            via: "".into(),
            confidence: 0,
            source: "user".into(),
            rejected: true,
            ..Default::default()
        }
    }

    #[test]
    fn merge_manifest_suppressed_by_user_rejected() {
        // A user-rejected (to=2, kind=lib) tombstone must suppress a fresh manifest
        // edge with the same (to, kind). User always wins over manifest.
        let existing = vec![user_tombstone(2, "lib")];
        let fresh_manifest = vec![manifest_rel(2, "lib", "acme-lib")];
        let merged = super::merge_relations(&existing, &fresh_manifest, &[]);
        assert!(merged.iter().any(|r| r.to == 2 && r.rejected), "tombstone survives");
        assert!(
            !merged.iter().any(|r| r.to == 2 && r.source == "manifest"),
            "manifest edge suppressed by user tombstone"
        );
    }

    #[test]
    fn merge_manifest_suppresses_agent_same_kind_not_other() {
        // A manifest lib edge to repo 3 suppresses an agent lib edge to repo 3,
        // but NOT an agent http edge to repo 3 (different kind).
        let fresh_manifest = vec![manifest_rel(3, "lib", "some-lib")];
        let fresh_agent = vec![
            super::AgentRelation {
                to: 3,
                kind: "lib".into(),
                via: "agent-lib-evidence".into(),
                confidence: 70,
                source: "agent".into(),
                rejected: false,
                ..Default::default()
            },
            super::AgentRelation {
                to: 3,
                kind: "http".into(),
                via: "GET /api".into(),
                confidence: 80,
                source: "agent".into(),
                rejected: false,
                ..Default::default()
            },
        ];
        let merged = super::merge_relations(&[], &fresh_manifest, &fresh_agent);
        // manifest lib edge is present
        assert!(
            merged.iter().any(|r| r.to == 3 && r.kind == "lib" && r.source == "manifest"),
            "manifest lib edge present"
        );
        // agent lib edge to same repo is suppressed by manifest
        assert!(
            !merged.iter().any(|r| r.to == 3 && r.kind == "lib" && r.source == "agent"),
            "agent lib edge suppressed by manifest lib edge"
        );
        // agent http edge survives (different kind)
        assert!(
            merged.iter().any(|r| r.to == 3 && r.kind == "http" && r.source == "agent"),
            "agent http edge survives — different kind from manifest lib"
        );
    }

    #[test]
    fn merge_agent_survives_without_user_or_manifest_claim() {
        // Agent edges to repos that have no user/manifest claim survive.
        // Use explicit source="agent" so the assertion is unambiguous.
        let fresh_agent = vec![
            super::AgentRelation {
                to: 5,
                kind: "grpc".into(),
                via: "Svc.Call".into(),
                confidence: 60,
                source: "agent".into(),
                rejected: false,
                ..Default::default()
            },
            super::AgentRelation {
                to: 6,
                kind: "queue".into(),
                via: "orders-topic".into(),
                confidence: 75,
                source: "agent".into(),
                rejected: false,
                ..Default::default()
            },
        ];
        let merged = super::merge_relations(&[], &[], &fresh_agent);
        assert!(
            merged.iter().any(|r| r.to == 5 && r.source == "agent"),
            "unclaimed agent grpc edge survives"
        );
        assert!(
            merged.iter().any(|r| r.to == 6 && r.source == "agent"),
            "unclaimed agent queue edge survives"
        );
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
