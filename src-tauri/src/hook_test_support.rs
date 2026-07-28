//! Test-only helpers for driving the generated ask-hook scripts (claude's
//! per-worktree `.weft-ask-hook.sh` and codex's global helper). Lives in one
//! place because both hooks share the same fail-closed tail — a second copy of
//! the runner would be free to drift from it, which is exactly the mirroring
//! this module exists to prevent.

/// A 127.0.0.1 port with nothing listening: bind an ephemeral port, learn its
/// number, then drop the listener. Beats hardcoding a port some service on a dev
/// box might actually be serving.
pub(crate) fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

/// Run a generated hook script the way the engine does: PreToolUse JSON on
/// stdin, decision JSON on stdout. Returns `(stdout, exit code)`.
///
/// Async, own process group, and reaped at the deadline — all three on purpose.
/// The script's own curl timeout is an HOUR, so an unbounded wait would hang the
/// suite. A bounded `spawn_blocking` variant would NOT be enough either: a
/// blocking task can't be cancelled, so the test would fail fast while its
/// `std::process::Child` kept the runtime (and the whole test binary) alive
/// until curl gave up. And killing only the direct child isn't enough either —
/// `curl` is bash's GRANDchild and would outlive it as an orphan. So the script
/// runs in its own process group and the deadline reaps that group with the same
/// primitive production uses (`proc_registry::kill_group`), on top of
/// `kill_on_drop` handling the direct child.
#[cfg(unix)]
pub(crate) async fn run_hook_script(
    script: &std::path::Path,
    cwd: &std::path::Path,
    payload: &str,
    limit: std::time::Duration,
) -> (String, Option<i32>) {
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg(script)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = cmd.spawn().unwrap();
    // pgid == pid for a freshly created group. Captured BEFORE the child is moved
    // into `wait_with_output`, which consumes it.
    let pgid = child.id().map(|pid| pid as i32);
    let mut stdin = child.stdin.take().unwrap();
    // A broken pipe here is the script BEHAVING CORRECTLY, not a test failure:
    // several of these hooks exit before reading stdin (the non-loopback route
    // returns without posting at all), so the write races a legitimate early
    // exit. Unwrapping made that race a flaky `Broken pipe` panic on loaded CI
    // runners while passing everywhere fast. What the tests assert is the
    // script's stdout and exit code, both captured below.
    let _ = stdin.write_all(payload.as_bytes()).await;
    drop(stdin); // EOF — curl's `--data-binary @-` reads until it
    match tokio::time::timeout(limit, child.wait_with_output()).await {
        Ok(out) => {
            let out = out.unwrap();
            (
                String::from_utf8_lossy(&out.stdout).to_string(),
                out.status.code(),
            )
        }
        Err(_) => {
            // Dropping the timed-out future already SIGKILLed bash (kill_on_drop);
            // sweep the group so its curl can't linger for the remaining hour.
            if let Some(pgid) = pgid {
                crate::proc_registry::kill_group(pgid);
            }
            panic!("hook did not exit within {limit:?} (process group reaped)");
        }
    }
}

/// Assert a hook script emitted exactly one well-formed PreToolUse decision and
/// exited 0, then hand back the `hookSpecificOutput` object. Exit status is part
/// of the contract: a NON-zero exit is reported as a hook error and the tool call
/// CONTINUES (Codex documents this), so "crash to deny" would still be fail-open.
#[cfg(unix)]
pub(crate) fn decision_of(stdout: &str, code: Option<i32>) -> serde_json::Value {
    assert_eq!(code, Some(0), "hook must exit 0; stdout={stdout:?}");
    let body: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("hook must print one decision JSON, got {stdout:?}: {e}"));
    assert_eq!(
        body["hookSpecificOutput"]["hookEventName"], "PreToolUse",
        "decision must carry the PreToolUse envelope: {body}"
    );
    body["hookSpecificOutput"].clone()
}

/// A decision-shaped body, as `hook_decision` serializes it (compact, no spaces).
///
/// `truncated` drops only the FINAL brace — deliberately the worst case for the
/// hook's gates rather than a random cut point: it still starts with `{`, still
/// ends with `}`, and still contains the verdict pair, so it slips past the
/// structural check AND the verdict match. Only curl's non-zero exit distinguishes
/// it from a real answer, which is exactly what makes it prove the `$rc` gate. (A
/// cut mid-reason would fail the `{`…`}` check too and prove nothing about `$rc`.)
pub(crate) fn decision_body(verdict: &str, truncated: bool) -> String {
    let whole = format!(
        "{{\"hookSpecificOutput\":{{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"{verdict}\",\"permissionDecisionReason\":\"canned\"}}}}"
    );
    match truncated {
        true => whole[..whole.len() - 1].to_string(),
        false => whole,
    }
}

/// A one-shot server that answers with EXACTLY `status_line` + `body`, then closes.
/// Used to drive the responses weft's own axum handler can't produce but a crashing
/// weft, a stale port, or something else on the port can: an error status carrying a
/// decision-shaped body, and a body cut off mid-answer (`content_length` larger than
/// what's actually written makes curl report a partial transfer, exit 18).
/// Returns the base URL; the connection task ends with the process.
pub(crate) async fn serve_raw_once(
    status_line: &'static str,
    body: String,
    content_length: usize,
) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        // Drain one read so curl isn't blocked writing its (tiny) request.
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf).await;
        let head =
            format!("{status_line}\r\nContent-Type: application/json\r\nContent-Length: {content_length}\r\n\r\n");
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.write_all(body.as_bytes()).await;
        let _ = sock.flush().await;
        // Dropping the socket here closes it — short of Content-Length when the
        // caller asked for a truncated body.
    });
    format!("http://127.0.0.1:{port}")
}

/// Wait for the Ask Bridge to register the hook's ask, then answer it as the
/// human would. Bounded so a hook that never reaches weft fails the test instead
/// of sitting on the script's hour-long curl timeout.
pub(crate) async fn answer_first_ask(asks: &crate::ask::AskRegistry, ans: crate::ask::Answer) {
    let step = std::time::Duration::from_millis(20);
    let mut waited = std::time::Duration::ZERO;
    while asks.open().is_empty() {
        tokio::time::sleep(step).await;
        waited += step;
        assert!(
            waited < std::time::Duration::from_secs(20),
            "the hook never reached weft's /ask endpoint"
        );
    }
    let id = asks.open()[0].id;
    assert!(asks.answer(id, ans), "the ask must accept the answer");
}
