//! Deterministic repo profiling + the cross-repo dependency graph (ARCHITECTURE
//! §4.9). This module is the cheap, agent-free engine: it reads manifests and
//! the README (never full code), infers a repo's role / stack / published &
//! declared package identifiers, and links consumers to producers across the
//! workspace. The semantic one-liner from the curator agent layers on top; this
//! is the floor that always works offline.

use std::path::Path;

/// Facts inferred from a cheap, read-only inspection of a repo directory.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RepoFacts {
    /// service | app | library | infra | docs | unknown
    pub role: String,
    /// e.g. ["node", "typescript"], ["rust"], ["go"]
    pub stack: Vec<String>,
    /// Best one-line description candidate (manifest description / README); may be "".
    pub summary: String,
    /// Identifiers this repo PUBLISHES (package / module name) — graph targets.
    pub published: Vec<String>,
    /// Declared dependency identifiers — graph sources.
    pub deps: Vec<String>,
}

/// A directed dependency edge: `from` consumes `to`, evidenced by `via`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Edge {
    pub from: i32,
    pub to: i32,
    pub via: String,
}

fn read(dir: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(rel)).ok()
}

fn exists(dir: &Path, rel: &str) -> bool {
    dir.join(rel).exists()
}

/// Infer facts from a repo directory by reading manifests + README only.
/// Never reads source beyond presence checks (main.rs / lib.rs / main.go).
pub fn infer_repo_facts(dir: &Path) -> RepoFacts {
    let mut f = RepoFacts::default();

    if let Some(raw) = read(dir, "package.json") {
        infer_node(&mut f, dir, &raw);
    } else if let Some(raw) = read(dir, "Cargo.toml") {
        infer_rust(&mut f, dir, &raw);
    } else if let Some(raw) = read(dir, "go.mod") {
        infer_go(&mut f, dir, &raw);
    } else if exists(dir, "pyproject.toml") || exists(dir, "setup.py") {
        f.stack.push("python".into());
    }

    if f.role.is_empty() {
        f.role = infer_fallback_role(dir);
    }
    if f.summary.is_empty() {
        if let Some(s) = readme_summary(dir) {
            f.summary = s;
        }
    }
    f
}

fn infer_node(f: &mut RepoFacts, dir: &Path, raw: &str) {
    f.stack.push("node".into());
    if exists(dir, "tsconfig.json") {
        f.stack.push("typescript".into());
    }
    let json: serde_json::Value = serde_json::from_str(raw).unwrap_or(serde_json::Value::Null);
    if let Some(name) = json.get("name").and_then(|v| v.as_str()) {
        f.published.push(name.to_string());
    }
    if let Some(desc) = json.get("description").and_then(|v| v.as_str()) {
        f.summary = desc.trim().to_string();
    }
    for key in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(obj) = json.get(key).and_then(|v| v.as_object()) {
            for dep in obj.keys() {
                f.deps.push(dep.clone());
            }
        }
    }
    if !f.stack.contains(&"typescript".to_string()) && f.deps.iter().any(|d| d == "typescript") {
        f.stack.push("typescript".into());
    }
    f.role = node_role(&json, &f.deps);
}

const FRONTEND: &[&str] = &[
    "react",
    "vue",
    "svelte",
    "next",
    "@angular/core",
    "solid-js",
    "vite",
];
const BACKEND: &[&str] = &[
    "express",
    "fastify",
    "koa",
    "@nestjs/core",
    "hono",
    "@hapi/hapi",
];

fn node_role(json: &serde_json::Value, deps: &[String]) -> String {
    let has = |set: &[&str]| deps.iter().any(|d| set.contains(&d.as_str()));
    if has(BACKEND) {
        return "service".into();
    }
    if has(FRONTEND) {
        return "app".into();
    }
    // A library publishes an entry point and ships no server/app framework.
    let lib_fields = ["main", "module", "exports", "types"];
    if lib_fields.iter().any(|k| json.get(k).is_some()) {
        return "library".into();
    }
    if json.get("bin").is_some() {
        return "service".into();
    }
    "app".into()
}

fn infer_rust(f: &mut RepoFacts, dir: &Path, raw: &str) {
    f.stack.push("rust".into());
    let doc: toml::Value = raw
        .parse()
        .unwrap_or(toml::Value::Table(Default::default()));
    if let Some(pkg) = doc.get("package") {
        if let Some(name) = pkg.get("name").and_then(|v| v.as_str()) {
            f.published.push(name.to_string());
        }
        if let Some(desc) = pkg.get("description").and_then(|v| v.as_str()) {
            f.summary = desc.trim().to_string();
        }
    }
    if let Some(deps) = doc.get("dependencies").and_then(|v| v.as_table()) {
        for dep in deps.keys() {
            f.deps.push(dep.clone());
        }
    }
    // A crate with a binary target is a runnable service/app; otherwise treat it
    // as a library (the common default for a bare or lib-only crate).
    let has_bin = exists(dir, "src/main.rs") || doc.get("bin").is_some();
    f.role = if has_bin {
        "service".into()
    } else {
        "library".into()
    };
}

fn infer_go(f: &mut RepoFacts, dir: &Path, raw: &str) {
    f.stack.push("go".into());
    let mut in_require = false;
    for line in raw.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("module ") {
            f.published.push(rest.trim().to_string());
        } else if l.starts_with("require (") {
            in_require = true;
        } else if in_require && l == ")" {
            in_require = false;
        } else if let Some(rest) = l.strip_prefix("require ") {
            if let Some(path) = rest.split_whitespace().next() {
                f.deps.push(path.to_string());
            }
        } else if in_require && !l.is_empty() {
            if let Some(path) = l.split_whitespace().next() {
                f.deps.push(path.to_string());
            }
        }
    }
    f.role = if exists(dir, "main.go") {
        "service".into()
    } else {
        "library".into()
    };
}

fn infer_fallback_role(dir: &Path) -> String {
    if exists(dir, "Dockerfile")
        || exists(dir, "docker-compose.yml")
        || exists(dir, "docker-compose.yaml")
        || exists(dir, "main.tf")
    {
        return "infra".into();
    }
    if exists(dir, "mkdocs.yml") || (exists(dir, "docs") && !exists(dir, "src")) {
        return "docs".into();
    }
    "unknown".into()
}

/// First real prose line of the README: skip headings, badges, and blanks.
fn readme_summary(dir: &Path) -> Option<String> {
    let raw = read(dir, "README.md").or_else(|| read(dir, "readme.md"))?;
    for line in raw.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with("![") || l.starts_with("[!") {
            continue;
        }
        let l = l.trim_start_matches(['>', '*', '-', ' ']);
        if l.is_empty() {
            continue;
        }
        return Some(l.chars().take(160).collect());
    }
    None
}

/// Link each consumer to each producer it declares a dependency on. An edge
/// exists when `from.deps ∩ to.published` is non-empty; self-edges are skipped.
pub fn compute_edges(repos: &[(i32, RepoFacts)]) -> Vec<Edge> {
    let mut edges = Vec::new();
    for (from_id, from) in repos {
        for (to_id, to) in repos {
            if from_id == to_id {
                continue;
            }
            if let Some(via) = from.deps.iter().find(|d| to.published.contains(d)) {
                edges.push(Edge {
                    from: *from_id,
                    to: *to_id,
                    via: via.clone(),
                });
            }
        }
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_repo(files: &[(&str, &str)]) -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("weft-prof-{}-{}", std::process::id(), id));
        std::fs::create_dir_all(&dir).unwrap();
        for (rel, content) in files {
            let p = dir.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }
        dir
    }

    #[test]
    fn node_app_with_typescript() {
        let dir = tmp_repo(&[
            (
                "package.json",
                r#"{ "name": "web-app", "description": "Checkout frontend",
                     "dependencies": { "react": "^18", "@acme/api-client": "1.0" } }"#,
            ),
            ("tsconfig.json", "{}"),
        ]);
        let f = super::infer_repo_facts(&dir);
        assert!(f.stack.contains(&"node".to_string()));
        assert!(f.stack.contains(&"typescript".to_string()));
        assert_eq!(f.summary, "Checkout frontend");
        assert!(f.published.contains(&"web-app".to_string()));
        assert!(f.deps.contains(&"react".to_string()));
        assert!(f.deps.contains(&"@acme/api-client".to_string()));
        assert_eq!(f.role, "app"); // react → frontend app
    }

    #[test]
    fn node_library_role() {
        let dir = tmp_repo(&[(
            "package.json",
            r#"{ "name": "@acme/shared", "main": "dist/index.js",
                 "dependencies": { "zod": "^3" } }"#,
        )]);
        let f = super::infer_repo_facts(&dir);
        assert_eq!(f.role, "library");
        assert!(f.published.contains(&"@acme/shared".to_string()));
    }

    #[test]
    fn rust_library() {
        let dir = tmp_repo(&[
            (
                "Cargo.toml",
                "[package]\nname = \"engine\"\ndescription = \"core engine\"\n\n[dependencies]\nserde = \"1\"\n",
            ),
            ("src/lib.rs", "// lib"),
        ]);
        let f = super::infer_repo_facts(&dir);
        assert_eq!(f.stack, vec!["rust".to_string()]);
        assert_eq!(f.role, "library");
        assert_eq!(f.summary, "core engine");
        assert!(f.published.contains(&"engine".to_string()));
        assert!(f.deps.contains(&"serde".to_string()));
    }

    #[test]
    fn rust_binary_is_service() {
        let dir = tmp_repo(&[
            (
                "Cargo.toml",
                "[package]\nname = \"api\"\n\n[dependencies]\naxum = \"0.7\"\n",
            ),
            ("src/main.rs", "fn main() {}"),
        ]);
        let f = super::infer_repo_facts(&dir);
        assert_eq!(f.role, "service");
    }

    #[test]
    fn go_module() {
        let dir = tmp_repo(&[
            (
                "go.mod",
                "module github.com/acme/gateway\n\ngo 1.22\n\nrequire (\n\tgithub.com/gin-gonic/gin v1.9.1\n)\n",
            ),
            ("main.go", "package main"),
        ]);
        let f = super::infer_repo_facts(&dir);
        assert_eq!(f.stack, vec!["go".to_string()]);
        assert!(f.published.contains(&"github.com/acme/gateway".to_string()));
        assert!(f.deps.contains(&"github.com/gin-gonic/gin".to_string()));
    }

    #[test]
    fn readme_summary_when_manifest_has_none() {
        let dir = tmp_repo(&[
            ("Cargo.toml", "[package]\nname = \"thing\"\n"),
            (
                "README.md",
                "# Thing\n\nA small utility for parsing logs.\n",
            ),
        ]);
        let f = super::infer_repo_facts(&dir);
        assert_eq!(f.summary, "A small utility for parsing logs.");
    }

    #[test]
    fn empty_dir_is_unknown() {
        let dir = tmp_repo(&[("notes.txt", "hi")]);
        let f = super::infer_repo_facts(&dir);
        assert_eq!(f.role, "unknown");
        assert!(f.stack.is_empty());
        assert!(f.published.is_empty());
    }

    #[test]
    fn edges_link_consumer_to_producer() {
        let web = RepoFacts {
            deps: vec!["@acme/api-client".into(), "react".into()],
            ..Default::default()
        };
        let api = RepoFacts {
            published: vec!["@acme/api-client".into()],
            ..Default::default()
        };
        let edges = super::compute_edges(&[(1, web), (2, api)]);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from, 1);
        assert_eq!(edges[0].to, 2);
        assert_eq!(edges[0].via, "@acme/api-client");
    }

    #[test]
    fn edges_ignore_self_and_externals() {
        let a = RepoFacts {
            deps: vec!["serde".into(), "self-pkg".into()],
            published: vec!["self-pkg".into()],
            ..Default::default()
        };
        let b = RepoFacts {
            published: vec!["b-pkg".into()],
            ..Default::default()
        };
        // a depends on serde (external) + itself; nothing in the workspace.
        let edges = super::compute_edges(&[(1, a), (2, b)]);
        assert!(edges.is_empty());
    }
}
