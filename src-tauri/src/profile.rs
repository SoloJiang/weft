//! Deterministic repo profiling + the cross-repo dependency graph (ARCHITECTURE
//! §4.9). This module is the cheap, agent-free engine: it reads manifests and
//! the README (never full code), infers a repo's role / stack / published &
//! declared package identifiers, and links consumers to producers across the
//! workspace. The semantic one-liner from the curator agent layers on top; this
//! is the floor that always works offline.

use std::collections::HashMap;
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
///
/// `kind`/`source`/`confidence` are `serde(default)` so payloads written before
/// they existed still deserialize. Manifest edges (deterministic floor) are
/// `kind="lib"`, `source="manifest"`, `confidence=100`; the agent curator adds
/// richer kinds (`http`/`grpc`/`queue`/`infra`) tagged `source="agent"`.
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

/// Union manifest + agent edges, deduped by (from, to, via): a manifest edge
/// (a declared fact) wins over an agent edge for the same triple, but distinct
/// relationships between the same pair (e.g. a runtime HTTP call vs a declared
/// package dep) both survive.
pub fn merge_edges(manifest: Vec<Edge>, agent: Vec<Edge>) -> Vec<Edge> {
    let mut seen: std::collections::HashSet<(i32, i32, String)> =
        manifest.iter().map(|e| (e.from, e.to, e.via.clone())).collect();
    let mut out = manifest;
    for e in agent {
        if seen.insert((e.from, e.to, e.via.clone())) {
            out.push(e);
        }
    }
    out
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
    } else if let Some(raw) = read(dir, "pom.xml") {
        infer_maven(&mut f, dir, &raw);
    } else if let Some(raw) = read(dir, "build.gradle").or_else(|| read(dir, "build.gradle.kts")) {
        infer_gradle(&mut f, dir, &raw);
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
    if let Some(desc) = json
        .get("description")
        .and_then(|v| v.as_str())
        .and_then(sanitize_summary)
    {
        f.summary = desc;
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
        if let Some(desc) = pkg
            .get("description")
            .and_then(|v| v.as_str())
            .and_then(sanitize_summary)
        {
            f.summary = desc;
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

fn infer_maven(f: &mut RepoFacts, dir: &Path, raw: &str) {
    push_unique(&mut f.stack, "java");
    push_unique(&mut f.stack, "maven");

    let Ok(doc) = roxmltree::Document::parse(raw) else {
        f.role = "library".into();
        return;
    };
    let root = doc.root_element();
    let root_group = maven_project_group(root, None);
    let mut service = maven_collect_project(f, root, root_group.as_deref());

    for module in maven_modules(root) {
        let module_pom = dir.join(module.trim()).join("pom.xml");
        let Some(module_raw) = std::fs::read_to_string(module_pom).ok() else {
            continue;
        };
        let Ok(module_doc) = roxmltree::Document::parse(&module_raw) else {
            continue;
        };
        service |= maven_collect_project(f, module_doc.root_element(), root_group.as_deref());
    }

    f.role = if service { "service" } else { "library" }.into();
}

fn maven_collect_project(
    f: &mut RepoFacts,
    root: roxmltree::Node<'_, '_>,
    fallback_group: Option<&str>,
) -> bool {
    let props = maven_properties(root);
    let artifact_raw = maven_child_text(root, "artifactId").unwrap_or_default();
    let group_raw = maven_project_group(root, fallback_group).unwrap_or_default();
    let artifact = resolve_maven_value(&artifact_raw, &props, &group_raw, &artifact_raw);
    let group = resolve_maven_value(&group_raw, &props, &group_raw, &artifact);
    let packaging = maven_child_text(root, "packaging").unwrap_or_default();

    if !group.is_empty() && !artifact.is_empty() {
        push_unique(&mut f.published, format!("{group}:{artifact}"));
    } else if !artifact.is_empty() {
        push_unique(&mut f.published, artifact.clone());
    }

    if f.summary.is_empty() {
        if let Some(desc) = maven_child_text(root, "description").and_then(|s| sanitize_summary(&s)) {
            f.summary = desc;
        }
    }

    let mut service = packaging == "war";
    if let Some(framework) = maven_project_service_plugin(root) {
        push_unique(&mut f.stack, framework);
        service = true;
    }
    for deps in maven_children(root, "dependencies") {
        for dep in maven_children(deps, "dependency") {
            let scope = maven_child_text(dep, "scope").unwrap_or_default();
            if scope == "test" {
                continue;
            }
            let dep_group = maven_child_text(dep, "groupId")
                .map(|s| resolve_maven_value(&s, &props, &group, &artifact))
                .unwrap_or_default();
            let dep_artifact = maven_child_text(dep, "artifactId")
                .map(|s| resolve_maven_value(&s, &props, &group, &artifact))
                .unwrap_or_default();
            if dep_group.is_empty() || dep_artifact.is_empty() {
                continue;
            }
            let coord = format!("{dep_group}:{dep_artifact}");
            if let Some(framework) = java_framework_of_coord(&coord) {
                push_unique(&mut f.stack, framework);
                service = true;
            }
            push_unique(&mut f.deps, coord);
        }
    }
    service
}

fn maven_project_group(root: roxmltree::Node<'_, '_>, fallback: Option<&str>) -> Option<String> {
    maven_child_text(root, "groupId")
        .or_else(|| maven_child(root, "parent").and_then(|p| maven_child_text(p, "groupId")))
        .or_else(|| fallback.map(String::from))
}

fn maven_project_service_plugin(root: roxmltree::Node<'_, '_>) -> Option<&'static str> {
    root.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "plugin")
        .find_map(|plugin| {
            let group = maven_child_text(plugin, "groupId").unwrap_or_default();
            let artifact = maven_child_text(plugin, "artifactId").unwrap_or_default();
            java_framework_of_coord(&format!("{group}:{artifact}"))
        })
}

fn maven_properties(root: roxmltree::Node<'_, '_>) -> HashMap<String, String> {
    let mut props = HashMap::new();
    if let Some(node) = maven_child(root, "properties") {
        for child in node.children().filter(|n| n.is_element()) {
            if let Some(text) = child.text().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                props.insert(child.tag_name().name().to_string(), text.to_string());
            }
        }
    }
    props
}

fn maven_modules(root: roxmltree::Node<'_, '_>) -> Vec<String> {
    maven_child(root, "modules")
        .map(|mods| {
            maven_children(mods, "module")
                .filter_map(|m| m.text().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn maven_child<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    name: &str,
) -> Option<roxmltree::Node<'a, 'input>> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name)
}

fn maven_children<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    name: &'a str,
) -> impl Iterator<Item = roxmltree::Node<'a, 'input>> + 'a {
    node.children()
        .filter(move |n| n.is_element() && n.tag_name().name() == name)
}

fn maven_child_text(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    maven_child(node, name)
        .and_then(|n| n.text())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn resolve_maven_value(
    raw: &str,
    props: &HashMap<String, String>,
    project_group: &str,
    project_artifact: &str,
) -> String {
    let mut out = raw.trim().to_string();
    let replacements = [
        ("project.groupId", project_group),
        ("pom.groupId", project_group),
        ("groupId", project_group),
        ("project.artifactId", project_artifact),
        ("pom.artifactId", project_artifact),
        ("artifactId", project_artifact),
    ];
    for (key, value) in replacements {
        out = out.replace(&format!("${{{key}}}"), value);
    }
    for _ in 0..2 {
        let mut changed = false;
        for (key, value) in props {
            let token = format!("${{{key}}}");
            if out.contains(&token) {
                out = out.replace(&token, value);
                changed = true;
            }
        }
        for (key, value) in replacements {
            let token = format!("${{{key}}}");
            if out.contains(&token) {
                out = out.replace(&token, value);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    out
}

fn infer_gradle(f: &mut RepoFacts, dir: &Path, raw: &str) {
    push_unique(&mut f.stack, "java");
    push_unique(&mut f.stack, "gradle");

    let settings = read(dir, "settings.gradle").or_else(|| read(dir, "settings.gradle.kts"));
    let root_name = settings
        .as_deref()
        .and_then(|s| gradle_assigned_value(s, "rootProject.name"))
        .or_else(|| {
            dir.file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "java-project".into());
    let root_group = gradle_assigned_value(raw, "group").unwrap_or_default();
    let root_artifact = gradle_artifact_id(raw).unwrap_or(root_name);
    gradle_collect_project(f, &root_group, &root_artifact, raw);
    let mut service = gradle_project_is_service(raw, &f.deps);
    if let Some(framework) = gradle_project_framework(raw, &f.deps) {
        push_unique(&mut f.stack, framework);
    }

    if let Some(settings_raw) = settings {
        for module in gradle_includes(&settings_raw) {
            let module_path = module.trim_matches(':').replace(':', "/");
            if module_path.is_empty() {
                continue;
            }
            let module_name = module_path
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(&module_path);
            if !root_group.is_empty() {
                push_unique(&mut f.published, format!("{root_group}:{module_name}"));
            }
            let module_dir = dir.join(&module_path);
            let Some(module_raw) = std::fs::read_to_string(module_dir.join("build.gradle"))
                .ok()
                .or_else(|| std::fs::read_to_string(module_dir.join("build.gradle.kts")).ok())
            else {
                continue;
            };
            let module_group = gradle_assigned_value(&module_raw, "group")
                .or_else(|| {
                    if root_group.is_empty() {
                        None
                    } else {
                        Some(root_group.clone())
                    }
                })
                .unwrap_or_default();
            gradle_collect_project(f, &module_group, module_name, &module_raw);
            service |= gradle_project_is_service(&module_raw, &f.deps);
            if let Some(framework) = gradle_project_framework(&module_raw, &f.deps) {
                push_unique(&mut f.stack, framework);
            }
        }
    }

    f.role = if service { "service" } else { "library" }.into();
}

fn gradle_collect_project(f: &mut RepoFacts, group: &str, name: &str, raw: &str) {
    if !group.is_empty() && !name.is_empty() {
        push_unique(&mut f.published, format!("{group}:{name}"));
    } else if !name.is_empty() {
        push_unique(&mut f.published, name.to_string());
    }
    for dep in gradle_dependencies(raw) {
        push_unique(&mut f.deps, dep);
    }
}

fn gradle_project_is_service(raw: &str, deps: &[String]) -> bool {
    gradle_project_framework(raw, deps).is_some()
        || raw.contains("id 'application'")
        || raw.contains("id \"application\"")
        || raw.contains("id(\"application\")")
        || raw.contains("plugins { application")
        || raw.contains("application {")
}

fn gradle_project_framework(raw: &str, deps: &[String]) -> Option<&'static str> {
    if raw.contains("id 'org.springframework.boot'")
        || raw.contains("id \"org.springframework.boot\"")
        || raw.contains("id(\"org.springframework.boot\")")
        || deps
            .iter()
            .any(|d| java_framework_of_coord(d) == Some("spring"))
    {
        return Some("spring");
    }
    if raw.contains("id 'io.quarkus'")
        || raw.contains("id \"io.quarkus\"")
        || raw.contains("id(\"io.quarkus\")")
        || deps
            .iter()
            .any(|d| java_framework_of_coord(d) == Some("quarkus"))
    {
        return Some("quarkus");
    }
    if raw.contains("id 'io.micronaut'")
        || raw.contains("id \"io.micronaut\"")
        || raw.contains("id(\"io.micronaut\")")
        || deps
            .iter()
            .any(|d| java_framework_of_coord(d) == Some("micronaut"))
    {
        return Some("micronaut");
    }
    None
}

fn gradle_dependencies(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = strip_gradle_comment(line).trim();
        if !is_gradle_dependency_line(line) {
            continue;
        }
        if let Some(coord) = first_quoted(line).and_then(|s| coord_from_gradle_notation(&s)) {
            push_unique(&mut out, coord);
            continue;
        }
        if let (Some(group), Some(name)) = (
            gradle_map_value(line, "group"),
            gradle_map_value(line, "name"),
        ) {
            push_unique(&mut out, format!("{group}:{name}"));
        }
    }
    out
}

fn is_gradle_dependency_line(line: &str) -> bool {
    const CONFIGS: &[&str] = &[
        "implementation",
        "api",
        "compileOnly",
        "runtimeOnly",
        "compile",
        "annotationProcessor",
        "kapt",
        "dependency",
    ];
    CONFIGS.iter().any(|config| {
        line.strip_prefix(config)
            .and_then(|rest| rest.chars().next())
            .map(|c| c.is_whitespace() || c == '(')
            .unwrap_or(false)
    })
}

fn coord_from_gradle_notation(s: &str) -> Option<String> {
    let mut parts = s.split(':');
    let group = parts.next()?.trim();
    let artifact = parts.next()?.trim();
    if group.is_empty() || artifact.is_empty() {
        return None;
    }
    Some(format!("{group}:{artifact}"))
}

fn gradle_includes(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = strip_gradle_comment(line).trim();
        if !line.starts_with("include") {
            continue;
        }
        for value in quoted_values(line) {
            push_unique(&mut out, value);
        }
    }
    out
}

fn gradle_assigned_value(raw: &str, key: &str) -> Option<String> {
    for line in raw.lines() {
        let line = strip_gradle_comment(line)
            .trim()
            .trim_end_matches(';')
            .trim();
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        if rest
            .chars()
            .next()
            .map(|c| c.is_whitespace() || c == '=' || c == '(')
            .unwrap_or(false)
        {
            if let Some(value) = first_quoted(rest) {
                return Some(value);
            }
        }
    }
    None
}

fn gradle_artifact_id(raw: &str) -> Option<String> {
    gradle_assigned_value(raw, "artifactId")
        .or_else(|| gradle_assigned_value(raw, "def artifactId"))
        .or_else(|| gradle_assigned_value(raw, "archivesBaseName"))
        .or_else(|| gradle_assigned_value(raw, "baseName"))
}

fn gradle_map_value(line: &str, key: &str) -> Option<String> {
    let needle = format!("{key}:");
    let idx = line.find(&needle)?;
    first_quoted(&line[idx + needle.len()..])
}

fn strip_gradle_comment(line: &str) -> &str {
    line.split_once("//").map(|(head, _)| head).unwrap_or(line)
}

fn first_quoted(s: &str) -> Option<String> {
    quoted_values(s).into_iter().next()
}

fn quoted_values(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.char_indices();
    while let Some((start, c)) = chars.next() {
        if c != '\'' && c != '"' {
            continue;
        }
        for (end, next) in chars.by_ref() {
            if next == c {
                out.push(s[start + c.len_utf8()..end].to_string());
                break;
            }
        }
    }
    out
}

fn java_framework_of_coord(coord: &str) -> Option<&'static str> {
    if coord.starts_with("org.springframework.boot:") || coord.contains(":spring-boot-starter") {
        return Some("spring");
    }
    if coord.starts_with("io.quarkus:") {
        return Some("quarkus");
    }
    if coord.starts_with("io.micronaut:") {
        return Some("micronaut");
    }
    None
}

fn push_unique(values: &mut Vec<String>, value: impl Into<String>) {
    let value = value.into();
    if !value.is_empty() && !values.contains(&value) {
        values.push(value);
    }
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

fn sanitize_summary(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(trimmed.len());
    let mut in_tag = false;
    let mut wrote_space = false;
    for c in trimmed.chars() {
        match c {
            '<' => {
                in_tag = true;
                if !out.is_empty() && !wrote_space {
                    out.push(' ');
                    wrote_space = true;
                }
            }
            '>' => in_tag = false,
            _ if in_tag => {}
            _ if c.is_whitespace() => {
                if !out.is_empty() && !wrote_space {
                    out.push(' ');
                    wrote_space = true;
                }
            }
            _ => {
                out.push(c);
                wrote_space = false;
            }
        }
    }

    let cleaned = out.trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.chars().take(160).collect())
    }
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
        if let Some(summary) = sanitize_summary(l) {
            return Some(summary);
        }
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
                    kind: "lib".into(),
                    source: "manifest".into(),
                    confidence: 100,
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
        write_tmp_files(&dir, files);
        dir
    }

    fn tmp_repo_named(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("weft-prof-{}-{id}", std::process::id()))
            .join(name);
        write_tmp_files(&dir, files);
        dir
    }

    fn write_tmp_files(dir: &std::path::Path, files: &[(&str, &str)]) {
        std::fs::create_dir_all(&dir).unwrap();
        for (rel, content) in files {
            let p = dir.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }
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
    fn maven_service_profile() {
        let dir = tmp_repo(&[(
            "pom.xml",
            r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.acme</groupId>
  <artifactId>checkout-service</artifactId>
  <version>1.0.0</version>
  <description>Checkout API service</description>
  <dependencies>
    <dependency>
      <groupId>org.springframework.boot</groupId>
      <artifactId>spring-boot-starter-web</artifactId>
    </dependency>
    <dependency>
      <groupId>com.acme</groupId>
      <artifactId>pricing-client</artifactId>
    </dependency>
  </dependencies>
</project>"#,
        )]);
        let f = super::infer_repo_facts(&dir);
        assert!(f.stack.contains(&"java".to_string()));
        assert!(f.stack.contains(&"maven".to_string()));
        assert!(f.stack.contains(&"spring".to_string()));
        assert_eq!(f.role, "service");
        assert_eq!(f.summary, "Checkout API service");
        assert!(f
            .published
            .contains(&"com.acme:checkout-service".to_string()));
        assert!(f
            .deps
            .contains(&"org.springframework.boot:spring-boot-starter-web".to_string()));
        assert!(f.deps.contains(&"com.acme:pricing-client".to_string()));
    }

    #[test]
    fn java_edges_link_maven_repos() {
        let consumer = RepoFacts {
            deps: vec!["com.acme:pricing-client".into()],
            ..Default::default()
        };
        let producer = RepoFacts {
            published: vec!["com.acme:pricing-client".into()],
            ..Default::default()
        };
        let edges = super::compute_edges(&[(1, consumer), (2, producer)]);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from, 1);
        assert_eq!(edges[0].to, 2);
        assert_eq!(edges[0].via, "com.acme:pricing-client");
    }

    #[test]
    fn gradle_service_profile() {
        let dir = tmp_repo(&[
            (
                "settings.gradle",
                "pluginManagement {}\ndependencyResolutionManagement {}\nrootProject.name = 'billing-service'\n",
            ),
            (
                "build.gradle",
                "plugins { id 'java' id 'org.springframework.boot' version '3.3.0' }\ngroup = 'com.acme'\ndependencies {\n  implementation 'com.acme:pricing-client:1.2.3'\n  testImplementation 'org.junit.jupiter:junit-jupiter:5.10.0'\n}\n",
            ),
        ]);
        let f = super::infer_repo_facts(&dir);
        assert!(f.stack.contains(&"java".to_string()));
        assert!(f.stack.contains(&"gradle".to_string()));
        assert!(f.stack.contains(&"spring".to_string()));
        assert_eq!(f.role, "service");
        assert!(f
            .published
            .contains(&"com.acme:billing-service".to_string()));
        assert!(f.deps.contains(&"com.acme:pricing-client".to_string()));
    }

    #[test]
    fn gradle_publishes_space_group_and_artifact_id() {
        let dir = tmp_repo(&[
            ("settings.gradle", "rootProject.name = 'redimcommon'\n"),
            (
                "build.gradle",
                "group 'com.xhs.redim.common'\ndef artifactId = \"redimcommon\"\ndependencies {\n  compile('com.xhs.redim.cache:redimcache:0.1.7')\n}\n",
            ),
        ]);
        let f = super::infer_repo_facts(&dir);
        assert!(f.published.contains(&"com.xhs.redim.common:redimcommon".to_string()));
        assert!(f.deps.contains(&"com.xhs.redim.cache:redimcache".to_string()));
    }

    #[test]
    fn gradle_parenthesized_group_publishes_modules_and_compile_deps() {
        let dir = tmp_repo_named(
            "redimgeneralbiz",
            &[
                (
                    "settings.gradle",
                    "include 'boot'\ninclude 'common'\ninclude 'domainapi'\n",
                ),
                (
                    "build.gradle",
                    "plugins { id 'org.springframework.boot' version '2.1.6.RELEASE' }\nsubprojects {\n  group(\"com.xhs.redim.biz\")\n}\ndependencies {\n  compile('com.xhs.redim.common:redimcommon')\n  compile('com.xhs.redim.cache:redimcache')\n}\n",
                ),
                ("common/build.gradle", "dependencies {}\n"),
                ("domainapi/build.gradle", "dependencies { compile(project(\":common\")) }\n"),
            ],
        );
        let f = super::infer_repo_facts(&dir);
        assert_eq!(f.role, "service");
        assert!(f.stack.contains(&"spring".to_string()));
        assert!(f.published.contains(&"com.xhs.redim.biz:common".to_string()));
        assert!(f.published.contains(&"com.xhs.redim.biz:domainapi".to_string()));
        assert!(f.deps.contains(&"com.xhs.redim.common:redimcommon".to_string()));
        assert!(f.deps.contains(&"com.xhs.redim.cache:redimcache".to_string()));
    }

    #[test]
    fn gradle_application_service_does_not_claim_spring() {
        let dir = tmp_repo(&[
            ("settings.gradle.kts", "rootProject.name = \"worker\"\n"),
            (
                "build.gradle.kts",
                "plugins { application }\ngroup = \"com.acme\"\n",
            ),
        ]);
        let f = super::infer_repo_facts(&dir);
        assert_eq!(f.role, "service");
        assert!(f.stack.contains(&"java".to_string()));
        assert!(f.stack.contains(&"gradle".to_string()));
        assert!(!f.stack.contains(&"spring".to_string()));
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
    fn readme_summary_skips_html_blocks() {
        let dir = tmp_repo(&[
            ("Cargo.toml", "[package]\nname = \"thing\"\n"),
            (
                "README.md",
                "# Thing\n\n<p align=\"center\"><img src=\"logo.svg\" /></p>\n\nA small utility for parsing logs.\n",
            ),
        ]);
        let f = super::infer_repo_facts(&dir);
        assert_eq!(f.summary, "A small utility for parsing logs.");
    }

    #[test]
    fn manifest_summary_strips_html_tags() {
        let dir = tmp_repo(&[(
            "package.json",
            r#"{ "name": "web-app", "description": "<p>Checkout frontend</p>",
                 "dependencies": { "react": "^18" } }"#,
        )]);
        let f = super::infer_repo_facts(&dir);
        assert_eq!(f.summary, "Checkout frontend");
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

    #[test]
    fn manifest_edges_are_tagged_lib_manifest_full_confidence() {
        let web = RepoFacts {
            deps: vec!["@acme/api-client".into()],
            ..Default::default()
        };
        let api = RepoFacts {
            published: vec!["@acme/api-client".into()],
            ..Default::default()
        };
        let edges = super::compute_edges(&[(1, web), (2, api)]);
        assert_eq!(edges.len(), 1);
        // A declared manifest dependency is a fact: kind=lib, source=manifest, 100.
        assert_eq!(edges[0].kind, "lib");
        assert_eq!(edges[0].source, "manifest");
        assert_eq!(edges[0].confidence, 100);
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
    fn merge_edges_prefers_manifest_and_keeps_distinct_kinds() {
        let manifest = vec![super::Edge {
            from: 1,
            to: 2,
            via: "@acme/api-client".into(),
            kind: "lib".into(),
            source: "manifest".into(),
            confidence: 100,
        }];
        let agent = vec![
            // Same (from,to,via) as a manifest edge → manifest wins, agent dropped.
            super::Edge {
                from: 1,
                to: 2,
                via: "@acme/api-client".into(),
                kind: "lib".into(),
                source: "agent".into(),
                confidence: 50,
            },
            // A genuinely different relationship (runtime HTTP) → kept.
            super::Edge {
                from: 1,
                to: 2,
                via: "GET /orders".into(),
                kind: "http".into(),
                source: "agent".into(),
                confidence: 80,
            },
        ];
        let merged = super::merge_edges(manifest, agent);
        assert_eq!(merged.len(), 2);
        // The surviving (1,2,@acme/api-client) edge is the manifest one.
        let lib = merged.iter().find(|e| e.via == "@acme/api-client").unwrap();
        assert_eq!(lib.source, "manifest");
        assert_eq!(lib.confidence, 100);
        assert!(merged.iter().any(|e| e.kind == "http" && e.source == "agent"));
    }
}
