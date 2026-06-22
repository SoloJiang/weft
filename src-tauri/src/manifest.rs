//! Multi-language dependency manifest parser.
//!
//! Turns on-disk manifests into `ManifestInfo { provides, requires }`.
//! Every parser takes file content as `&str` and returns empty on any parse
//! error — never panics.

use std::path::Path;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct ManifestInfo {
    pub provides: Vec<String>,
    pub requires: Vec<String>,
}

impl ManifestInfo {
    fn merge(&mut self, other: ManifestInfo) {
        self.provides.extend(other.provides);
        self.requires.extend(other.requires);
    }

    fn dedup(&mut self) {
        self.provides.sort();
        self.provides.dedup();
        self.requires.sort();
        self.requires.dedup();
    }
}

// ---------------------------------------------------------------------------
// package.json
// ---------------------------------------------------------------------------

pub fn parse_package_json(s: &str) -> ManifestInfo {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(s) else {
        return ManifestInfo::default();
    };
    let mut info = ManifestInfo::default();

    if let Some(name) = v.get("name").and_then(|n| n.as_str()) {
        if !name.is_empty() {
            info.provides.push(name.to_string());
        }
    }

    for key in &[
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(obj) = v.get(key).and_then(|d| d.as_object()) {
            for dep_name in obj.keys() {
                info.requires.push(dep_name.clone());
            }
        }
    }

    info.dedup();
    info
}

// ---------------------------------------------------------------------------
// Cargo.toml
// ---------------------------------------------------------------------------

pub fn parse_cargo_toml(s: &str) -> ManifestInfo {
    let Ok(val) = s.parse::<toml::Value>() else {
        return ManifestInfo::default();
    };
    let mut info = ManifestInfo::default();

    if let Some(name) = val
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
    {
        if !name.is_empty() {
            info.provides.push(name.to_string());
        }
    }

    for section in &[
        "dependencies",
        "dev-dependencies",
        "build-dependencies",
    ] {
        if let Some(table) = val.get(section).and_then(|t| t.as_table()) {
            for key in table.keys() {
                info.requires.push(key.clone());
            }
        }
    }

    // workspace.dependencies
    if let Some(table) = val
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(|t| t.as_table())
    {
        for key in table.keys() {
            info.requires.push(key.clone());
        }
    }

    info.dedup();
    info
}

// ---------------------------------------------------------------------------
// go.mod
// ---------------------------------------------------------------------------

pub fn parse_go_mod(s: &str) -> ManifestInfo {
    let mut info = ManifestInfo::default();
    let mut in_require_block = false;

    for line in s.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("module ") {
            let module_path = trimmed.trim_start_matches("module ").trim();
            if !module_path.is_empty() {
                info.provides.push(module_path.to_string());
            }
            continue;
        }

        if trimmed == "require (" {
            in_require_block = true;
            continue;
        }

        if trimmed == ")" && in_require_block {
            in_require_block = false;
            continue;
        }

        if in_require_block {
            // lines like: "\tgithub.com/acme/lib v1.2.0"
            if let Some(path) = trimmed.split_whitespace().next() {
                if !path.is_empty() && path != "//" {
                    info.requires.push(path.to_string());
                }
            }
            continue;
        }

        // single-line: "require github.com/foo/bar v1.0.0"
        if let Some(rest) = trimmed.strip_prefix("require ") {
            if let Some(path) = rest.split_whitespace().next() {
                if !path.is_empty() {
                    info.requires.push(path.to_string());
                }
            }
        }
    }

    info.dedup();
    info
}

// ---------------------------------------------------------------------------
// pyproject.toml
// ---------------------------------------------------------------------------

/// Strip a PEP 508 specifier down to the bare package name.
/// E.g. `"acme-shared==1.0"` → `"acme-shared"`.
fn pep508_name(spec: &str) -> &str {
    // Split at any of: space, <, >, =, !, ~, [, ;
    let end = spec
        .find(|c: char| matches!(c, ' ' | '<' | '>' | '=' | '!' | '~' | '[' | ';'))
        .unwrap_or(spec.len());
    &spec[..end]
}

/// PEP 503 normalisation: lowercase, collapse runs of `-`/`_`/`.` to a single `-`.
fn normalize_python_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut result = String::with_capacity(lower.len());
    let mut last_was_sep = false;
    for ch in lower.chars() {
        if matches!(ch, '-' | '_' | '.') {
            if !last_was_sep {
                result.push('-');
            }
            last_was_sep = true;
        } else {
            result.push(ch);
            last_was_sep = false;
        }
    }
    result
}

pub fn parse_pyproject(s: &str) -> ManifestInfo {
    let Ok(val) = s.parse::<toml::Value>() else {
        return ManifestInfo::default();
    };
    let mut info = ManifestInfo::default();

    // PEP 621: [project].name and [project].dependencies
    if let Some(project) = val.get("project") {
        if let Some(name) = project.get("name").and_then(|n| n.as_str()) {
            let norm = normalize_python_name(name);
            if !norm.is_empty() {
                info.provides.push(norm);
            }
        }
        if let Some(deps) = project.get("dependencies").and_then(|d| d.as_array()) {
            for item in deps {
                if let Some(spec) = item.as_str() {
                    let raw = pep508_name(spec);
                    let norm = normalize_python_name(raw);
                    if !norm.is_empty() {
                        info.requires.push(norm);
                    }
                }
            }
        }
    }

    // Poetry: [tool.poetry].name and [tool.poetry.dependencies]
    if let Some(poetry) = val
        .get("tool")
        .and_then(|t| t.get("poetry"))
    {
        if let Some(name) = poetry.get("name").and_then(|n| n.as_str()) {
            let norm = normalize_python_name(name);
            if !norm.is_empty() && !info.provides.contains(&norm) {
                info.provides.push(norm);
            }
        }
        if let Some(table) = poetry.get("dependencies").and_then(|d| d.as_table()) {
            for key in table.keys() {
                if key != "python" {
                    info.requires.push(normalize_python_name(key));
                }
            }
        }
    }

    info.dedup();
    info
}

// ---------------------------------------------------------------------------
// pom.xml
// ---------------------------------------------------------------------------

/// Extract the text of a named immediate child element.
fn pom_child_text<'a>(node: roxmltree::Node<'a, 'a>, tag: &str) -> Option<String> {
    for child in node.children() {
        if child.tag_name().name() == tag {
            if let Some(text) = child.text() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

/// Build a `groupId:artifactId` coordinate from a POM node.
/// Falls back to bare `artifactId` when `groupId` is absent.
fn pom_coord(node: roxmltree::Node) -> Option<String> {
    let artifact = pom_child_text(node, "artifactId")?;
    let coord = match pom_child_text(node, "groupId") {
        Some(group) => format!("{group}:{artifact}"),
        None => artifact,
    };
    Some(coord)
}

/// Extract the groupId from a `<parent>` element, if present.
fn pom_parent_group(root: roxmltree::Node) -> Option<String> {
    for child in root.children() {
        if child.tag_name().name() == "parent" {
            return pom_child_text(child, "groupId");
        }
    }
    None
}

pub fn parse_pom_xml(s: &str) -> ManifestInfo {
    let Ok(doc) = roxmltree::Document::parse(s) else {
        return ManifestInfo::default();
    };
    let mut info = ManifestInfo::default();

    let root = doc.root_element();

    // Build the project's effective provides coordinate.
    // When own <groupId> is absent, inherit from <parent><groupId>.
    if let Some(artifact) = pom_child_text(root, "artifactId") {
        let group = pom_child_text(root, "groupId")
            .or_else(|| pom_parent_group(root));
        let coord = match group {
            Some(g) => format!("{g}:{artifact}"),
            None => artifact,
        };
        info.provides.push(coord);
    }

    // Find <dependencies> → <dependency> → groupId:artifactId
    for child in root.children() {
        if child.tag_name().name() == "dependencies" {
            for dep in child.children() {
                if dep.tag_name().name() == "dependency" {
                    if let Some(coord) = pom_coord(dep) {
                        info.requires.push(coord);
                    }
                }
            }
        }
    }

    info.dedup();
    info
}

// ---------------------------------------------------------------------------
// build.gradle / build.gradle.kts
// ---------------------------------------------------------------------------

/// Extract the `group:artifact` coordinate from a Gradle dependency string like `"g:a:v"`.
fn gradle_dep_name(raw: &str) -> Option<String> {
    // Strip surrounding quotes
    let inner = raw
        .trim()
        .trim_start_matches(|c| c == '\'' || c == '"')
        .trim_end_matches(|c| c == '\'' || c == '"');
    let parts: Vec<&str> = inner.split(':').collect();
    if parts.len() >= 2 {
        let group = parts[0].trim();
        let artifact = parts[1].trim();
        if !group.is_empty() && !artifact.is_empty() {
            return Some(format!("{group}:{artifact}"));
        }
    }
    None
}

pub fn parse_gradle(s: &str) -> ManifestInfo {
    let mut info = ManifestInfo::default();

    let dep_keywords = [
        "implementation",
        "api",
        "compile",
        "testImplementation",
        "runtimeOnly",
        "compileOnly",
        "testRuntimeOnly",
        "testCompileOnly",
        "annotationProcessor",
        "kapt",
    ];

    for line in s.lines() {
        let trimmed = line.trim();
        // Skip comment lines
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            continue;
        }

        for kw in &dep_keywords {
            if !trimmed.starts_with(kw) {
                continue;
            }
            let rest = &trimmed[kw.len()..];
            let rest = rest.trim_start_matches(|c: char| c == ' ' || c == '(');
            // Find a quoted string
            if let Some(q_start) = rest.find(|c| c == '\'' || c == '"') {
                // `find` returns a BYTE offset; index bytes (the quote is ASCII), not
                // chars — `chars().nth(byte_offset)` would mis-pick after multi-byte text.
                let quote_char = rest.as_bytes()[q_start] as char;
                let after_open = &rest[q_start + 1..];
                if let Some(q_end) = after_open.find(quote_char) {
                    let coord = &after_open[..q_end];
                    if let Some(name) = gradle_dep_name(&format!("'{coord}'")) {
                        info.requires.push(name);
                    }
                }
            }
            break;
        }
    }

    info.dedup();
    info
}

/// Best-effort: read `rootProject.name` from settings.gradle[.kts] and
/// `group` from build.gradle[.kts], then emit `group:name` (or bare `name`).
pub fn parse_gradle_provides(settings_src: &str, build_src: &str) -> Vec<String> {
    let mut name: Option<String> = None;
    let mut group: Option<String> = None;

    for line in settings_src.lines() {
        let t = line.trim();
        if t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
            continue;
        }
        // rootProject.name = "x"  or  rootProject.name 'x'
        if let Some(rest) = t.strip_prefix("rootProject.name") {
            let rest = rest.trim_start_matches(|c: char| c == ' ' || c == '=' || c == '(');
            let value = rest
                .trim_start_matches(|c| c == '\'' || c == '"')
                .trim_end_matches(|c: char| c == '\'' || c == '"' || c == ')' || c == ';');
            let value = value.trim();
            if !value.is_empty() {
                name = Some(value.to_string());
                break;
            }
        }
    }

    for line in build_src.lines() {
        let t = line.trim();
        if t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
            continue;
        }
        // group = "x"  or  group "x"  or  group 'x'
        if let Some(rest) = t.strip_prefix("group") {
            let rest = rest.trim_start_matches(|c: char| c == ' ' || c == '=' || c == '(');
            let value = rest
                .trim_start_matches(|c| c == '\'' || c == '"')
                .trim_end_matches(|c: char| c == '\'' || c == '"' || c == ')' || c == ';');
            let value = value.trim();
            if !value.is_empty() {
                group = Some(value.to_string());
                break;
            }
        }
    }

    match (group, name) {
        (Some(g), Some(n)) => vec![format!("{g}:{n}")],
        (None, Some(n)) => vec![n],
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// scan_repo
// ---------------------------------------------------------------------------

fn read_and_parse(path: &Path) -> ManifestInfo {
    let Ok(content) = std::fs::read_to_string(path) else {
        return ManifestInfo::default();
    };
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    match name {
        "package.json" => parse_package_json(&content),
        "Cargo.toml" => parse_cargo_toml(&content),
        "go.mod" => parse_go_mod(&content),
        "pyproject.toml" => parse_pyproject(&content),
        "pom.xml" => parse_pom_xml(&content),
        "build.gradle" | "build.gradle.kts" => parse_gradle(&content),
        _ => ManifestInfo::default(),
    }
}

/// Collect manifests from a single directory (non-recursive).
fn scan_dir(dir: &Path, result: &mut ManifestInfo) {
    let manifest_names = [
        "package.json",
        "Cargo.toml",
        "go.mod",
        "pyproject.toml",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
    ];
    for name in &manifest_names {
        let p = dir.join(name);
        if p.exists() {
            result.merge(read_and_parse(&p));
        }
    }
}

/// Shallow monorepo dirs to check one level into.
const MONOREPO_DIRS: &[&str] = &["packages", "apps", "services", "crates"];

/// Returns true if `path` is itself a symlink (does not follow the link).
fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

pub fn scan_repo(repo_root: &Path) -> ManifestInfo {
    let mut result = ManifestInfo::default();

    // Top-level manifests
    scan_dir(repo_root, &mut result);

    // Gradle provides at the repo root (settings.gradle[.kts] + build.gradle[.kts]).
    let settings_content = ["settings.gradle", "settings.gradle.kts"]
        .iter()
        .find_map(|n| std::fs::read_to_string(repo_root.join(n)).ok())
        .unwrap_or_default();
    let build_content = ["build.gradle", "build.gradle.kts"]
        .iter()
        .find_map(|n| std::fs::read_to_string(repo_root.join(n)).ok())
        .unwrap_or_default();
    if !settings_content.is_empty() || !build_content.is_empty() {
        for p in parse_gradle_provides(&settings_content, &build_content) {
            result.provides.push(p);
        }
    }

    // One level into standard monorepo sub-dirs
    for subdir_name in MONOREPO_DIRS {
        let subdir = repo_root.join(subdir_name);
        // Finding 1: skip when the container dir itself is a symlink.
        if is_symlink(&subdir) {
            continue;
        }
        if !subdir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&subdir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Skip symlinked entries — they may point outside the repo tree.
            if is_symlink(&path) {
                continue;
            }
            if path.is_dir() {
                let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if dir_name.starts_with('@') {
                    // Finding 5: npm scoped dir — descend one more level.
                    let Ok(scoped_entries) = std::fs::read_dir(&path) else {
                        continue;
                    };
                    for scoped_entry in scoped_entries.flatten() {
                        let scoped_path = scoped_entry.path();
                        if is_symlink(&scoped_path) {
                            continue;
                        }
                        if scoped_path.is_dir() {
                            scan_dir(&scoped_path, &mut result);
                        }
                    }
                } else {
                    scan_dir(&path, &mut result);
                }
            }
        }
    }

    result.dedup();
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_json_name_and_deps() {
        let s = r#"{"name":"@acme/web","dependencies":{"@acme/ui":"^1","react":"^18"},
            "devDependencies":{"@acme/test":"^1"}}"#;
        let m = parse_package_json(s);
        assert!(m.provides.contains(&"@acme/web".to_string()));
        assert!(m.requires.contains(&"@acme/ui".to_string()));
        assert!(m.requires.contains(&"@acme/test".to_string()));
        assert!(m.requires.contains(&"react".to_string()));
    }

    #[test]
    fn package_json_peer_and_optional_deps() {
        let s = r#"{"name":"pkg","peerDependencies":{"react":">=17"},
            "optionalDependencies":{"fsevents":"*"}}"#;
        let m = parse_package_json(s);
        assert!(m.requires.contains(&"react".to_string()));
        assert!(m.requires.contains(&"fsevents".to_string()));
    }

    #[test]
    fn cargo_toml_package_and_deps() {
        let s = "[package]\nname = \"acme-core\"\n[dependencies]\nacme-util = \"1\"\nserde = \"1\"\n";
        let m = parse_cargo_toml(s);
        assert!(m.provides.contains(&"acme-core".to_string()));
        assert!(m.requires.contains(&"acme-util".to_string()));
        assert!(m.requires.contains(&"serde".to_string()));
    }

    #[test]
    fn cargo_toml_dev_and_build_deps() {
        let s = "[package]\nname = \"foo\"\n[dev-dependencies]\ncriterion = \"0.5\"\n[build-dependencies]\ncc = \"1\"\n";
        let m = parse_cargo_toml(s);
        assert!(m.requires.contains(&"criterion".to_string()));
        assert!(m.requires.contains(&"cc".to_string()));
    }

    #[test]
    fn go_mod_module_and_require() {
        let s = "module github.com/acme/svc\n\nrequire (\n\tgithub.com/acme/lib v1.2.0\n\tgithub.com/x/y v0.1.0\n)\n";
        let m = parse_go_mod(s);
        assert!(m.provides.contains(&"github.com/acme/svc".to_string()));
        assert!(m.requires.contains(&"github.com/acme/lib".to_string()));
        assert!(m.requires.contains(&"github.com/x/y".to_string()));
    }

    #[test]
    fn go_mod_single_line_require() {
        let s = "module github.com/foo/bar\nrequire github.com/baz/qux v2.0.0\n";
        let m = parse_go_mod(s);
        assert!(m.provides.contains(&"github.com/foo/bar".to_string()));
        assert!(m.requires.contains(&"github.com/baz/qux".to_string()));
    }

    #[test]
    fn pyproject_pep621_name_and_deps() {
        let s = "[project]\nname = \"acme_api\"\ndependencies = [\"acme-shared==1.0\", \"fastapi>=0.1\"]\n";
        let m = parse_pyproject(s);
        // PEP 503: acme_api normalises to acme-api
        assert!(m.provides.contains(&"acme-api".to_string()));
        assert!(m.requires.iter().any(|r| r == "acme-shared"));
        assert!(m.requires.iter().any(|r| r == "fastapi"));
    }

    #[test]
    fn pyproject_poetry_name_and_deps() {
        let s = "[tool.poetry]\nname = \"my-lib\"\n[tool.poetry.dependencies]\npython = \"^3.11\"\nrequests = \"^2\"\n";
        let m = parse_pyproject(s);
        assert!(m.provides.contains(&"my-lib".to_string()));
        assert!(m.requires.contains(&"requests".to_string()));
        // python should be excluded
        assert!(!m.requires.contains(&"python".to_string()));
    }

    #[test]
    fn pom_artifact_and_deps() {
        let s = r#"<project><groupId>com.acme</groupId><artifactId>core</artifactId>
            <dependencies><dependency><groupId>com.acme</groupId><artifactId>util</artifactId></dependency>
            </dependencies></project>"#;
        let m = parse_pom_xml(s);
        // Full coordinates expected
        assert!(
            m.provides.contains(&"com.acme:core".to_string()),
            "provides must be full coord: {:?}",
            m.provides
        );
        assert!(
            m.requires.contains(&"com.acme:util".to_string()),
            "requires must be full coord: {:?}",
            m.requires
        );
    }

    #[test]
    fn pom_full_coords_no_collision() {
        // Two artifacts with the same artifactId but different groupIds must NOT collide.
        let provides_pom = r#"<project><groupId>com.acme</groupId><artifactId>core</artifactId>
            <dependencies></dependencies></project>"#;
        let requires_pom = r#"<project><groupId>org.other</groupId><artifactId>core</artifactId>
            <dependencies><dependency><groupId>org.other</groupId><artifactId>core</artifactId></dependency>
            </dependencies></project>"#;
        let mp = parse_pom_xml(provides_pom);
        let mr = parse_pom_xml(requires_pom);
        // com.acme:core should NOT appear in org.other requires
        assert!(!mr.requires.contains(&"com.acme:core".to_string()));
        assert!(mr.requires.contains(&"org.other:core".to_string()));
        assert!(mp.provides.contains(&"com.acme:core".to_string()));
    }

    #[test]
    fn pom_fallback_no_group_id() {
        // When groupId is absent, fall back to bare artifactId.
        let s = r#"<project><artifactId>mylib</artifactId><dependencies></dependencies></project>"#;
        let m = parse_pom_xml(s);
        assert!(
            m.provides.contains(&"mylib".to_string()),
            "bare artifactId fallback must work: {:?}",
            m.provides
        );
    }

    #[test]
    fn gradle_implementation_deps() {
        let s = r#"
plugins { id 'java' }
dependencies {
    implementation 'com.acme:core:1.0'
    testImplementation "junit:junit:4.13"
    api 'com.acme:util:2.0'
}
"#;
        let m = parse_gradle(s);
        // Full group:artifact coordinates expected
        assert!(
            m.requires.contains(&"com.acme:core".to_string()),
            "requires must be full coord: {:?}",
            m.requires
        );
        assert!(
            m.requires.contains(&"junit:junit".to_string()),
            "requires must be full coord: {:?}",
            m.requires
        );
        assert!(
            m.requires.contains(&"com.acme:util".to_string()),
            "requires must be full coord: {:?}",
            m.requires
        );
    }

    #[test]
    fn gradle_full_coords_no_collision() {
        // Two jars with the same artifactId from different groups must not collide.
        let s = r#"
dependencies {
    implementation 'com.acme:core:1.0'
    implementation 'org.other:core:2.0'
}
"#;
        let m = parse_gradle(s);
        assert!(m.requires.contains(&"com.acme:core".to_string()));
        assert!(m.requires.contains(&"org.other:core".to_string()));
        // Bare "core" must not appear
        assert!(!m.requires.contains(&"core".to_string()));
    }

    #[test]
    fn malformed_inputs_degrade_to_empty() {
        assert!(parse_package_json("{ not json").requires.is_empty());
        assert!(parse_cargo_toml("[[[").requires.is_empty());
        assert!(parse_pom_xml("<x>").requires.is_empty());
        // Also confirm provides is empty for these
        assert!(parse_package_json("{ not json").provides.is_empty());
        assert!(parse_go_mod("").provides.is_empty());
        assert!(parse_pyproject("!!!invalid toml!!!").provides.is_empty());
        assert!(parse_gradle("not a dep line").requires.is_empty());
    }

    #[test]
    fn scan_repo_temp_dir() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("weft_manifest_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        fs::write(
            tmp.join("package.json"),
            r#"{"name":"my-app","dependencies":{"lodash":"^4"}}"#,
        )
        .unwrap();

        // monorepo member under packages/
        let pkg_dir = tmp.join("packages").join("shared");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(
            pkg_dir.join("Cargo.toml"),
            "[package]\nname = \"shared-crate\"\n[dependencies]\nserde = \"1\"\n",
        )
        .unwrap();

        let info = scan_repo(&tmp);

        assert!(info.provides.contains(&"my-app".to_string()));
        assert!(info.requires.contains(&"lodash".to_string()));
        assert!(info.provides.contains(&"shared-crate".to_string()));
        assert!(info.requires.contains(&"serde".to_string()));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scan_repo_missing_root_returns_empty() {
        let missing = Path::new("/nonexistent/path/that/does/not/exist");
        let info = scan_repo(missing);
        assert!(info.provides.is_empty());
        assert!(info.requires.is_empty());
    }

    #[test]
    fn dedup_is_applied() {
        // Two manifests providing the same name should deduplicate.
        let s1 = r#"{"name":"pkg","dependencies":{"react":"^17","react":"^18"}}"#;
        let m = parse_package_json(s1);
        // serde_json deduplicates object keys, and our dedup handles Vec level
        let react_count = m.requires.iter().filter(|r| *r == "react").count();
        assert_eq!(react_count, 1);
    }

    // -----------------------------------------------------------------------
    // Finding 1: symlinked CONTAINER dir excluded
    // -----------------------------------------------------------------------
    #[test]
    #[cfg(unix)]
    fn scan_repo_symlink_container_excluded() {
        use std::fs;
        use std::os::unix::fs::symlink;

        let tmp = std::env::temp_dir()
            .join(format!("weft_symlink_container_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // An external dir that acts as the "packages" tree.
        let external = tmp.join("external_packages");
        fs::create_dir_all(external.join("comp")).unwrap();
        fs::write(
            external.join("comp").join("package.json"),
            r#"{"name":"container-secret","dependencies":{}}"#,
        )
        .unwrap();

        // Make packages/ itself a symlink → external.
        symlink(&external, tmp.join("packages")).unwrap();

        let info = scan_repo(&tmp);

        assert!(
            !info.provides.contains(&"container-secret".to_string()),
            "symlinked container must not be descended: {:?}",
            info.provides
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    // -----------------------------------------------------------------------
    // Finding 2: Maven parent groupId resolution
    // -----------------------------------------------------------------------
    #[test]
    fn pom_inherits_group_from_parent() {
        let s = r#"<project>
            <parent><groupId>com.acme</groupId><artifactId>parent</artifactId></parent>
            <artifactId>core</artifactId>
            <dependencies></dependencies>
        </project>"#;
        let m = parse_pom_xml(s);
        assert!(
            m.provides.contains(&"com.acme:core".to_string()),
            "provides must inherit parent groupId: {:?}",
            m.provides
        );
    }

    // -----------------------------------------------------------------------
    // Finding 3: PEP 503 name normalisation
    // -----------------------------------------------------------------------
    #[test]
    fn python_pep503_normalisation() {
        // provides: acme_shared → acme-shared
        let provides_src = "[project]\nname = \"acme_shared\"\n";
        let mp = parse_pyproject(provides_src);
        assert!(
            mp.provides.contains(&"acme-shared".to_string()),
            "provides must be normalised: {:?}",
            mp.provides
        );

        // requires: Acme.Shared → acme-shared
        let requires_src =
            "[project]\nname = \"other\"\ndependencies = [\"Acme.Shared>=1.0\"]\n";
        let mr = parse_pyproject(requires_src);
        assert!(
            mr.requires.contains(&"acme-shared".to_string()),
            "requires must be normalised: {:?}",
            mr.requires
        );
    }

    // -----------------------------------------------------------------------
    // Finding 4: Gradle provides
    // -----------------------------------------------------------------------
    #[test]
    fn gradle_provides_group_and_name() {
        let settings = "rootProject.name = \"core\"\n";
        let build = "group = \"com.acme\"\nversion = \"1.0\"\n";
        let provides = parse_gradle_provides(settings, build);
        assert!(
            provides.contains(&"com.acme:core".to_string()),
            "gradle provides must be group:name: {:?}",
            provides
        );
    }

    #[test]
    fn gradle_provides_name_only() {
        let settings = "rootProject.name = \"mylib\"\n";
        let provides = parse_gradle_provides(settings, "");
        assert!(
            provides.contains(&"mylib".to_string()),
            "gradle provides bare name when no group: {:?}",
            provides
        );
    }

    // -----------------------------------------------------------------------
    // Finding 5: scoped npm package dirs
    // -----------------------------------------------------------------------
    #[test]
    fn scan_repo_scoped_npm_packages() {
        use std::fs;

        let tmp = std::env::temp_dir()
            .join(format!("weft_scoped_npm_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // packages/@acme/ui/package.json
        let ui_dir = tmp.join("packages").join("@acme").join("ui");
        fs::create_dir_all(&ui_dir).unwrap();
        fs::write(
            ui_dir.join("package.json"),
            r#"{"name":"@acme/ui","dependencies":{"react":"^18"}}"#,
        )
        .unwrap();

        let info = scan_repo(&tmp);

        assert!(
            info.provides.contains(&"@acme/ui".to_string()),
            "scoped package must be found: {:?}",
            info.provides
        );
        assert!(
            info.requires.contains(&"react".to_string()),
            "scoped package deps must be found: {:?}",
            info.requires
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    /// scan_repo must NOT follow symlinked subdirectories under monorepo dirs.
    #[test]
    #[cfg(unix)]
    fn scan_repo_symlink_subdir_excluded() {
        use std::fs;
        use std::os::unix::fs::symlink;

        let tmp = std::env::temp_dir()
            .join(format!("weft_symlink_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // External dir with its own manifest — must NOT be included.
        let external = tmp.join("external_pkg");
        fs::create_dir_all(&external).unwrap();
        fs::write(
            external.join("package.json"),
            r#"{"name":"external-secret","dependencies":{}}"#,
        )
        .unwrap();

        // packages/<x> is a symlink pointing to the external dir.
        let packages_dir = tmp.join("packages");
        fs::create_dir_all(&packages_dir).unwrap();
        symlink(&external, packages_dir.join("linked")).unwrap();

        // A real (non-symlink) package — must be included.
        let real_pkg = packages_dir.join("real-pkg");
        fs::create_dir_all(&real_pkg).unwrap();
        fs::write(
            real_pkg.join("package.json"),
            r#"{"name":"real-pkg","dependencies":{}}"#,
        )
        .unwrap();

        let info = scan_repo(&tmp);

        assert!(
            !info.provides.contains(&"external-secret".to_string()),
            "symlinked external manifest must not be included: {:?}",
            info.provides
        );
        assert!(
            info.provides.contains(&"real-pkg".to_string()),
            "real (non-symlink) package must be included: {:?}",
            info.provides
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
