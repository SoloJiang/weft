//! Executable verification (ARCHITECTURE §4.13): "worker done = checks green,
//! not self-report." Infer a repo's cheap→expensive check rungs from its
//! manifest, run them in a worktree, and report structured pass/fail. This is
//! the floor of the trust dashboard; bounded retry + escalation build on it.

use serde::Serialize;
use std::path::Path;
use std::process::Command;

/// One verification rung: a named command to run in the worktree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CheckResult {
    pub name: String,
    /// "pass" | "fail"
    pub status: String,
    pub code: i32,
    /// Last lines of combined output (truncated).
    pub output_tail: String,
}

fn check(name: &str, program: &str, args: &[&str]) -> Check {
    Check {
        name: name.to_string(),
        program: program.to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
    }
}

/// Infer the check rungs for a repo dir, cheapest first. Node uses whichever of
/// typecheck/lint/build/test the package.json actually defines (no invented
/// scripts); rust and go use their standard cheap+test pair. Unknown → none.
pub fn infer_checks(dir: &Path) -> Vec<Check> {
    let mut out = Vec::new();

    // language rungs (cheap → expensive), one toolchain per repo dir
    if let Ok(raw) = std::fs::read_to_string(dir.join("package.json")) {
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
        let scripts = json.get("scripts").and_then(|v| v.as_object());
        let has = |k: &str| scripts.map(|s| s.contains_key(k)).unwrap_or(false);
        // Honor the repo's package manager (from its lockfile) — running `npm` in
        // a pnpm/yarn repo misfires. `<pm> run <script>` is uniform across all
        // three, and `<pm> test -- --run` forwards the flag to the test runner.
        let pm = if dir.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if dir.join("yarn.lock").exists() {
            "yarn"
        } else {
            "npm"
        };
        if has("typecheck") {
            out.push(check("typecheck", pm, &["run", "typecheck"]));
        }
        if has("lint") {
            out.push(check("lint", pm, &["run", "lint"]));
        }
        if has("build") {
            out.push(check("build", pm, &["run", "build"]));
        }
        if has("test") {
            out.push(check("test", pm, &["test", "--", "--run"]));
        }
    } else if dir.join("Cargo.toml").exists() {
        out.push(check("check", "cargo", &["check", "--quiet"]));
        out.push(check("test", "cargo", &["test", "--quiet"]));
    } else if dir.join("go.mod").exists() {
        out.push(check("build", "go", &["build", "./..."]));
        out.push(check("test", "go", &["test", "./..."]));
    } else if dir.join("pyproject.toml").exists()
        || dir.join("setup.py").exists()
        || dir.join("requirements.txt").exists()
    {
        // Python has no single canonical runner, so — like Node's scripts — only
        // add a rung when the repo actually CONFIGURES that tool (config file or a
        // [tool.*] table), never an invented default. Cheap → expensive.
        let pyproject = std::fs::read_to_string(dir.join("pyproject.toml")).unwrap_or_default();
        let uses_ruff = dir.join("ruff.toml").exists()
            || dir.join(".ruff.toml").exists()
            || pyproject.contains("[tool.ruff");
        if uses_ruff {
            out.push(check("lint", "ruff", &["check", "."]));
        }
        let uses_mypy = dir.join("mypy.ini").exists() || pyproject.contains("[tool.mypy");
        if uses_mypy {
            out.push(check("typecheck", "mypy", &["."]));
        }
        let uses_pytest = dir.join("pytest.ini").exists()
            || dir.join("conftest.py").exists()
            || pyproject.contains("[tool.pytest");
        if uses_pytest {
            out.push(check("test", "pytest", &["-q"]));
        }
    }

    // contract rung (§4.13 interface-contract conformance): a proto/buf repo gets
    // `buf lint`, additive alongside the language rungs above. Convention-detected
    // like the toolchains — only fires when the repo already uses buf.
    if ["buf.yaml", "buf.yml", "buf.gen.yaml"]
        .iter()
        .any(|f| dir.join(f).exists())
    {
        out.push(check("contract", "buf", &["lint"]));
    }

    out
}

/// Keep the last ~`max` bytes of `s`, on a line boundary when possible.
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.trim_end().to_string();
    }
    let start = s.len() - max;
    let slice = &s[start..];
    let slice = slice.find('\n').map(|i| &slice[i + 1..]).unwrap_or(slice);
    format!("…\n{}", slice.trim_end())
}

/// Run one check in `cwd`, capturing exit code + an output tail. A spawn failure
/// (tool missing) is reported as a fail with the error, never a panic.
pub fn run_check(cwd: &Path, c: &Check) -> CheckResult {
    let out = Command::new(&c.program)
        .args(&c.args)
        .current_dir(cwd)
        // Checks run user tooling (e.g. nvm's `npm`) that a GUI launch's minimal
        // PATH can't resolve; use the augmented PATH (see detect::tool_path).
        .env("PATH", crate::detect::tool_path())
        .output();
    match out {
        Ok(o) => {
            let mut combined = String::from_utf8_lossy(&o.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&o.stderr));
            let code = o.status.code().unwrap_or(-1);
            CheckResult {
                name: c.name.clone(),
                status: if o.status.success() { "pass" } else { "fail" }.to_string(),
                code,
                output_tail: tail(&combined, 2000),
            }
        }
        Err(e) => CheckResult {
            name: c.name.clone(),
            status: "fail".to_string(),
            code: -1,
            output_tail: format!("could not run {}: {e}", c.program),
        },
    }
}

/// Infer + run all check rungs for a worktree.
pub fn run_checks(cwd: &Path) -> Vec<CheckResult> {
    infer_checks(cwd)
        .iter()
        .map(|c| run_check(cwd, c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp(files: &[(&str, &str)]) -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("weft-check-{}-{}", std::process::id(), id));
        let _ = std::fs::create_dir_all(&d);
        for (rel, c) in files {
            std::fs::write(d.join(rel), c).unwrap();
        }
        d
    }

    #[test]
    fn node_infers_only_defined_scripts_cheap_first() {
        let d = tmp(&[(
            "package.json",
            r#"{ "scripts": { "test": "vitest", "build": "tsc", "typecheck": "tsc --noEmit" } }"#,
        )]);
        let checks = infer_checks(&d);
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        // typecheck before build before test; lint absent (not defined)
        assert_eq!(names, vec!["typecheck", "build", "test"]);
    }

    #[test]
    fn node_honors_package_manager_from_lockfile() {
        let pnpm = tmp(&[
            (
                "package.json",
                r#"{ "scripts": { "lint": "eslint", "test": "vitest" } }"#,
            ),
            ("pnpm-lock.yaml", ""),
        ]);
        assert!(infer_checks(&pnpm).iter().all(|c| c.program == "pnpm"));

        let yarn = tmp(&[
            ("package.json", r#"{ "scripts": { "lint": "eslint" } }"#),
            ("yarn.lock", ""),
        ]);
        assert_eq!(infer_checks(&yarn)[0].program, "yarn");

        // no lockfile → npm
        let npm = tmp(&[("package.json", r#"{ "scripts": { "lint": "eslint" } }"#)]);
        assert_eq!(infer_checks(&npm)[0].program, "npm");
    }

    #[test]
    fn rust_repo_uses_cargo_check_then_test() {
        let d = tmp(&[("Cargo.toml", "[package]\nname=\"x\"\n")]);
        let names: Vec<String> = infer_checks(&d).into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["check", "test"]);
    }

    #[test]
    fn unknown_repo_has_no_checks() {
        let d = tmp(&[("readme.txt", "hi")]);
        assert!(infer_checks(&d).is_empty());
    }

    #[test]
    fn python_infers_only_configured_tools_cheap_first() {
        let d = tmp(&[(
            "pyproject.toml",
            "[tool.ruff]\nline-length = 100\n[tool.mypy]\nstrict = true\n[tool.pytest.ini_options]\n",
        )]);
        let checks = infer_checks(&d);
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["lint", "typecheck", "test"]);
        assert_eq!(checks[0].program, "ruff");
        assert_eq!(checks[1].program, "mypy");
        assert_eq!(checks[2].program, "pytest");
    }

    #[test]
    fn python_without_tool_config_has_no_checks() {
        // A bare Python repo (deps but no ruff/mypy/pytest config) gets NO rungs —
        // we never invent a runner, same discipline as Node's defined-scripts-only.
        let d = tmp(&[
            ("requirements.txt", "requests\n"),
            ("pyproject.toml", "[project]\nname='x'\n"),
        ]);
        assert!(infer_checks(&d).is_empty());
        // conftest.py alone is enough evidence of pytest, though.
        let d2 = tmp(&[
            ("setup.py", "from setuptools import setup\n"),
            ("conftest.py", ""),
        ]);
        let names: Vec<String> = infer_checks(&d2).into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["test"]);
    }

    #[test]
    fn proto_repo_adds_a_contract_rung_alongside_language() {
        // buf alone → just the contract rung
        let d = tmp(&[("buf.yaml", "version: v1\n")]);
        let names: Vec<String> = infer_checks(&d).into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["contract"]);
        // buf + a language → language rungs, then contract
        let d2 = tmp(&[("go.mod", "module x\n"), ("buf.yaml", "version: v1\n")]);
        let names2: Vec<String> = infer_checks(&d2).into_iter().map(|c| c.name).collect();
        assert_eq!(names2, vec!["build", "test", "contract"]);
    }

    #[test]
    fn run_check_reports_pass_and_fail() {
        let d = tmp(&[]);
        let pass = run_check(&d, &check("ok", "sh", &["-c", "echo hi; exit 0"]));
        assert_eq!(pass.status, "pass");
        assert_eq!(pass.code, 0);
        let fail = run_check(&d, &check("bad", "sh", &["-c", "echo boom 1>&2; exit 3"]));
        assert_eq!(fail.status, "fail");
        assert_eq!(fail.code, 3);
        assert!(fail.output_tail.contains("boom"));
    }

    #[test]
    fn missing_tool_is_a_fail_not_a_panic() {
        let d = tmp(&[]);
        let r = run_check(&d, &check("x", "weft-nonexistent-binary-xyz", &[]));
        assert_eq!(r.status, "fail");
        assert!(r.output_tail.contains("could not run"));
    }
}
