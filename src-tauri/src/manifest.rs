//! Multi-language dependency manifest parser.
//!
//! Turns on-disk manifests into `ManifestInfo { provides, requires }`.
//! Every parser takes file content as `&str` and returns empty on any parse
//! error — never panics.

use std::path::{Path, PathBuf};

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

pub fn parse_pyproject(s: &str) -> ManifestInfo {
    let Ok(val) = s.parse::<toml::Value>() else {
        return ManifestInfo::default();
    };
    let mut info = ManifestInfo::default();

    // PEP 621: [project].name and [project].dependencies
    if let Some(project) = val.get("project") {
        if let Some(name) = project.get("name").and_then(|n| n.as_str()) {
            if !name.is_empty() {
                info.provides.push(name.to_string());
            }
        }
        if let Some(deps) = project.get("dependencies").and_then(|d| d.as_array()) {
            for item in deps {
                if let Some(spec) = item.as_str() {
                    let name = pep508_name(spec);
                    if !name.is_empty() {
                        info.requires.push(name.to_string());
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
            if !name.is_empty() && !info.provides.contains(&name.to_string()) {
                info.provides.push(name.to_string());
            }
        }
        if let Some(table) = poetry.get("dependencies").and_then(|d| d.as_table()) {
            for key in table.keys() {
                if key != "python" {
                    info.requires.push(key.clone());
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

/// Extract `artifactId` text from the immediate children of a node.
fn pom_artifact_id(node: roxmltree::Node) -> Option<String> {
    for child in node.children() {
        if child.tag_name().name() == "artifactId" {
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

pub fn parse_pom_xml(s: &str) -> ManifestInfo {
    let Ok(doc) = roxmltree::Document::parse(s) else {
        return ManifestInfo::default();
    };
    let mut info = ManifestInfo::default();

    let root = doc.root_element();
    // The root <project> element — provides is its artifactId
    if let Some(artifact_id) = pom_artifact_id(root) {
        info.provides.push(artifact_id);
    }

    // Find <dependencies> → <dependency> → artifactId
    for child in root.children() {
        if child.tag_name().name() == "dependencies" {
            for dep in child.children() {
                if dep.tag_name().name() == "dependency" {
                    if let Some(artifact_id) = pom_artifact_id(dep) {
                        info.requires.push(artifact_id);
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

/// Extract the artifact name from a Gradle dependency string like `"g:a:v"`.
/// Returns the `a` (artifactId) segment.
fn gradle_dep_name(raw: &str) -> Option<String> {
    // Strip surrounding quotes
    let inner = raw
        .trim()
        .trim_start_matches(|c| c == '\'' || c == '"')
        .trim_end_matches(|c| c == '\'' || c == '"');
    let parts: Vec<&str> = inner.split(':').collect();
    if parts.len() >= 2 {
        let artifact = parts[1].trim();
        if !artifact.is_empty() {
            return Some(artifact.to_string());
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
                let quote_char = rest.chars().nth(q_start).unwrap_or('"');
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

pub fn scan_repo(repo_root: &Path) -> ManifestInfo {
    let mut result = ManifestInfo::default();

    // Top-level manifests
    scan_dir(repo_root, &mut result);

    // One level into standard monorepo sub-dirs
    for subdir_name in MONOREPO_DIRS {
        let subdir = repo_root.join(subdir_name);
        if !subdir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&subdir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                scan_dir(&path, &mut result);
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
        assert!(m.provides.contains(&"acme_api".to_string()));
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
        assert!(m.provides.iter().any(|p| p.contains("core")));
        assert!(m.requires.iter().any(|r| r.contains("util")));
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
        assert!(m.requires.contains(&"core".to_string()));
        assert!(m.requires.contains(&"junit".to_string()));
        assert!(m.requires.contains(&"util".to_string()));
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
}
