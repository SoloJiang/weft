//! `weft_computer` MCP server. M1 shipped observation only
//! (window enumeration + screenshot); M2 adds input injection — click/type/
//! key/scroll/drag/move — gated behind the SAME `computer_use_enabled` app
//! setting (default OFF, fails closed — see `computer::enabled`), PLUS a
//! process-wide control lock, an open-permission-card check, and an input
//! throttle, so at most one session drives the human's real mouse/keyboard
//! at a time. This does NOT touch `inject.rs` / `commands.rs` / `engine.rs`
//! — those back a DIFFERENT feature (an engine's own PTY input).
//!
//! Wiring mirrors `bus::server::handle` (JSON-RPC over one SSE
//! `event: message` per POST). Deliberately does NOT reach into
//! `bus::server`'s private helpers across modules — `text_result`/`sse` are
//! copied here instead, matching `bus::global`'s existing precedent for a
//! new MCP server module.
//!
//! Every `computer` call result comes back as plain text.
//!
//! Architecture: `/computer/:thread/:dir/mcp`'s
//! identity comes from the URL path alone (same guarantee `bus::server::
//! handle` relies on) — but unlike the OTHER bus MCP servers, this one used
//! to have NO approval gate of its own at all: authorization lived entirely
//! in the calling engine's PreToolUse hook, a layer ANY local process
//! (including the agent itself, e.g. a raw `curl` straight into this
//! endpoint) can simply skip. [`run_action`]'s new [`approve`] step closes
//! that gap: it gates EVERY `tools/call` here, server-side, before it can
//! reach a backend — a Needs-you card the human answers through the exact
//! same `AskRegistry` the hook path uses, just via a route the calling
//! engine cannot route around. `bus::server::AUTO_APPROVED_INTERNAL_TOOLS`
//! now auto-approves `weft_computer`'s hook ask (see its own doc) so a
//! well-behaved engine's hook doesn't ALSO card the same call a second time.
//!
//! ONE tool, `computer`, dispatched by an `action` argument — the same shape
//! as Anthropic's `computer_20251124`, OpenAI's Responses `computer`, and
//! omp's `computer` tool, which models are already trained against.

use crate::ask::{AskRegistry, Decision};
use crate::computer::{self, backend, backend::MouseButton, ComputerError};
use crate::store::{repo, Db};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

// —— move every synchronous OS/encode step off the async runtime ——

/// run one synchronous OS/encode step on
/// tokio's blocking pool instead of directly on the async worker that called
/// it. Before this change, EVERY OS-touching call this module makes —
/// `computer::visible_windows`/`resolve_window` (xcap window enumeration),
/// `screenshot_window` (capture + PNG encode), `encode_jpeg_data_uri` (JPEG
/// re-encode, preview and MCP image alike), `activate_target` (a shell-out
/// that can block for a real amount of wall-clock time), and every input
/// backend call (`click`/`type_text`/`key`/`scroll`/`drag`/`move_cursor`, all
/// backed by `enigo`) — ran straight on whichever Tokio worker thread picked
/// up the request. A session holding a standing Full/Always grant could fire
/// enough concurrent `list_windows`/input calls to occupy every worker in the
/// runtime's pool at once; worse, since the Tauri `Stop` command and the
/// global-Escape callback ALSO spawn onto that same pool, a single slow or
/// wedged OS call anywhere on this path could leave the kill switch itself
/// unable to get scheduled — exactly the failure mode a kill switch exists to
/// never have. Routing each such step through `spawn_blocking` keeps the
/// async workers free for Stop/Escape regardless of how long, how many, or
/// how stuck the OS-facing calls get; the dedicated blocking pool is sized to
/// grow for exactly this kind of workload. Failure here (`JoinError`, in
/// practice only ever a panic inside `f` or the runtime shutting down) is
/// mapped to a plain error string and propagated — fail closed, never a
/// panic that could unwind past this boundary.
async fn on_blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("desktop backend task failed: {e}"))
}

fn text_result(s: String) -> Value {
    json!({ "content": [{ "type": "text", "text": s }] })
}

/// A FAILED `tools/call`'s result — the same text content, plus the MCP
/// `isError: true` flag.
/// Without it, a denied permission / disabled setting / lost lease / backend
/// failure came back as an ordinary content-only result: MCP clients report
/// the call as successful and the tool-result pipeline persists/renders a
/// COMPLETED row, so error-aware behavior in the calling agent never fires
/// even though no requested action occurred.
fn error_result(s: String) -> Value {
    json!({ "content": [{ "type": "text", "text": s }], "isError": true })
}

/// A `screenshot` result whose owning session's engine is known to accept an
/// inline MCP `image` content block (see [`engine_accepts_mcp_image`]) —
/// `s` is the SAME confirmation text [`text_result`] alone would carry,
/// `image_b64` is the screenshot re-encoded as JPEG (no `data:` prefix — raw
/// base64, the shape the MCP `image` content type wants) via
/// [`computer::encode_jpeg_data_uri`]. The text path is NEVER dropped even
/// when the image is attached: every engine, image-
/// capable or not, still gets the file path as a fallback for its own
/// image-viewing tool.
fn text_and_image_result(s: String, image_b64: String) -> Value {
    json!({ "content": [
        { "type": "text", "text": s },
        { "type": "image", "data": image_b64, "mimeType": "image/jpeg" },
    ] })
}

fn sse(value: Value) -> Response {
    let body = format!("event: message\ndata: {}\n\n", value);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

/// The `?wt=` query param's three distinguishable states
/// — CLAUDE.md: derive ONE discriminated value,
/// map it exhaustively, rather than re-deriving "is this absent, valid, or
/// garbage" ad hoc. Before this type existed, `q.get("wt").and_then(|s|
/// s.parse::<i32>().ok())` collapsed BOTH "no `?wt=` at all" and "a `?wt=`
/// present but not a number" onto the identical `None` — so a malformed/
/// forged non-numeric `wt` on a multi-repo direction took the SAME "first
/// worktree" fallback an honestly-absent one gets, silently misdirecting a
/// worker's screenshots/audit into a DIFFERENT repo's checkout. Resolution
/// already closes this for an explicit NUMERIC id that doesn't resolve to a
/// worktree of this direction (fail closed, never fall back to first); this
/// closes the identical gap one parse step earlier, for a `wt` that isn't
/// even numeric to begin with.
enum WtParam {
    /// No `?wt=` in the URL at all — the pre-existing "first worktree for
    /// this direction" fallback in `session_root` is unchanged.
    Absent,
    /// `?wt=<n>` parsed to a valid i32 — may still fail closed later, in
    /// `session_root`, if `n` doesn't name a worktree of THIS direction
    /// ; this variant only proves the STRING was numeric.
    Explicit(i32),
    /// `?wt=<garbage>` present but NOT a valid i32 — an EXPLICIT pin that is
    /// simply malformed. Must reject the call outright, never fall back to
    /// "first worktree": whatever worktree the caller meant to pin, a
    /// non-numeric value manifestly isn't "no preference at all", so
    /// silently guessing on its behalf reopens the exact misdirection
    /// the explicit-pin validation already refuses for a well-formed-but-wrong id.
    Invalid,
}

impl WtParam {
    fn parse(q: &HashMap<String, String>) -> WtParam {
        match q.get("wt") {
            None => WtParam::Absent,
            Some(s) => match s.parse::<i32>() {
                Ok(id) => WtParam::Explicit(id),
                Err(_) => WtParam::Invalid,
            },
        }
    }

    /// Collapse to what `session_root` already knows how to resolve
    /// (`Ok(Option<i32>)`, honoring the explicit pin's closed-set fail-
    /// closed rule), or a fixed rejection for `Invalid` — the exhaustive map
    /// CLAUDE.md asks for, kept in ONE place rather than an `if` scattered at
    /// the call site.
    fn resolve(&self) -> Result<Option<i32>, String> {
        match self {
            WtParam::Absent => Ok(None),
            WtParam::Explicit(id) => Ok(Some(*id)),
            WtParam::Invalid => Err(
                "invalid wt query parameter — it must be a worktree id (a plain integer); omit it \
                 entirely to fall back to this direction's first worktree"
                    .to_string(),
            ),
        }
    }
}

// —— per-session bearer for this ONE privileged
// endpoint ——
//
// Every OTHER bus MCP endpoint (`/bus/:thread/:dir/mcp`, `/planner/:thread/
// mcp`, …) relies on the URL path alone for identity — an accepted, deliberate
// local-first tradeoff for tools that only ever READ/steer a chat session (see
// `bus::server`'s own top-of-file doc comment). `/computer/:thread/:dir/mcp` is
// different in kind, not just degree: a POST that reaches [`call_computer`]
// can capture the human's real screen and inject real mouse/keyboard input —
// and unlike the bus endpoints, this one had NO caller-side authentication of
// its own at all. The URL this route lives at is itself injected into a
// worker's OWN MCP config/launch args (`inject::computer_url`) — readable by
// that same worker's process (or, same-uid, by ANY other local process that
// can read that worker's own config/environment) — so ANY local process that
// can read (or simply guess: the path is just `/computer/<thread>/<dir>/mcp`,
// two small integers/strings) could POST directly into this endpoint,
// impersonating an arbitrary `(thread, dir)` identity. If that identity
// happens to already hold a Full/exact-Always grant for some GUI action,
// `auto_decision_gui` in [`approve`] would silently wave the forged call
// through with no card at all — Weft would screenshot/click/type with its own
// desktop permissions on a forged caller's behalf.
//
// The fix: a per-session, unguessable bearer token bound to the EXACT
// `(thread, dir)` in the URL path, checked BEFORE the request's `method`/`id`
// are even inspected and before ANY authorization logic runs — see
// [`verify_computer_token`]. Deliberately NOT a persisted per-session secret
// (no new store table, no migration): the token is a keyed HMAC of the path's
// own `(thread, dir)` under a process-lifetime random secret, so verifying it
// needs nothing more than recomputing the SAME HMAC from the SAME path values
// — no lookup table to keep in sync, no secret to ever write to disk.
//
// The token additionally binds a per-identity GENERATION rotated at every
// fresh injection (see [`session_token_generations`]): a worker rerun,
// resumed under a new persisted session, or switched to another engine gets
// a NEW bearer, and the child it replaced — which would otherwise share the
// same `(thread, dir, wt)` MAC forever — stops verifying the moment the
// replacement is minted.
//
// KNOWN, ACCEPTED residual (not eliminated this change): a SAME-UID local
// process can still read a legitimate worker's own MCP config file / process
// environment / launch args and recover this SAME token from there — this
// closes "any local process, any uid, can forge the path or guess a URL",
// not "no same-uid process can ever recover this session's own credential".
// That is the same-uid isolation ceiling this repo's other residuals already
// live with (nothing here is weaker than the rest of the process's own trust
// boundary) — full closure needs OS-level uid/sandbox isolation between
// weft and the tool processes it launches, tracked (like the other real-
// machine residuals in this file) as follow-up work, not
// something a single-process HMAC can fix on its own.

/// The process-wide HMAC key, generated ONCE per process from a CSPRNG
/// (`rand::rngs::OsRng` — already in this crate's dependency tree, see
/// `Cargo.toml`; this reuses it rather than adding a `getrandom`-direct
/// dependency) the first time this is called, and NEVER regenerated,
/// persisted, logged, or placed into any injected config — it lives ONLY in
/// this process's memory for its own lifetime. A restart mints a brand-new
/// secret (invalidating every previously-issued token, which is fine: a
/// fresh process re-injects fresh URLs with fresh tokens for every session it
/// spawns — nothing here needs to survive a restart).
///
/// `None` when the OS RNG cannot provide entropy: `try_fill_bytes` instead
/// of `fill_bytes`, whose failure path PANICS — this is a production path
/// (every token mint/verify lands here), and a host with a broken/exhausted
/// entropy source must degrade to "computer-use auth refused" (both callers
/// fail closed on `None`), never to tearing the whole app down. The failure
/// is deliberately NOT cached: only a SUCCESSFUL draw is stored, so a
/// transient RNG failure heals on the next call instead of disabling
/// computer use for the process's remaining lifetime.
fn computer_endpoint_secret() -> Option<&'static [u8; 32]> {
    static SECRET: OnceLock<[u8; 32]> = OnceLock::new();
    if let Some(secret) = SECRET.get() {
        return Some(secret);
    }
    use rand::RngCore;
    let mut buf = [0u8; 32];
    if rand::rngs::OsRng.try_fill_bytes(&mut buf).is_err() {
        return None;
    }
    // A concurrent first call may have won the race — `get_or_init` keeps
    // exactly one secret either way.
    Some(SECRET.get_or_init(|| buf))
}

type HmacSha256 = Hmac<Sha256>;

/// Monotonic per-identity token generation, folded into
/// [`computer_token_mac`] — the revocation half of the bearer scheme. Each
/// fresh injection for a `(thread, dir, wt)` identity rotates its generation
/// ([`rotate_computer_session_token`], called by `bus::inject`'s minting
/// paths right before the new token is rendered), and verification always
/// recomputes the MAC under the CURRENT generation — so the moment a worker
/// is rerun, resumed as a new persisted session, or switched to a different
/// engine, the replacement's own injection invalidates every token minted
/// for the child it replaces. The old child (or any process that kept its
/// injected config) gets a bare 401 at [`handle_computer`]'s entry gate,
/// BEFORE the liveness check, the control lease, or any standing
/// Full/Always grant is ever consulted — a stale-session bearer can no
/// longer ride shared grants into desktop actions alongside its
/// replacement. In-memory only, like the HMAC secret itself: a process
/// restart resets generations AND mints a fresh secret, so no pre-restart
/// token survives either way, and an identity never injected in this
/// process verifies at the default generation its own mint used.
fn session_token_generations() -> &'static Mutex<HashMap<(i32, String, Option<i32>), u64>> {
    static GENERATIONS: OnceLock<Mutex<HashMap<(i32, String, Option<i32>), u64>>> = OnceLock::new();
    GENERATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The CURRENT generation for one identity — read by
/// [`computer_token_mac`] on every mint AND every verify, never bumped here.
fn session_token_generation(thread: i32, dir: &str, wt: Option<i32>) -> u64 {
    session_token_generations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(thread, dir.to_string(), wt))
        .copied()
        .unwrap_or(0)
}

/// Invalidate every previously-minted bearer for `(thread, dir, wt)` AND
/// render the replacement's token, in ONE critical section on
/// [`session_token_generations`] — the injection paths
/// (`bus::inject::inject_computer` and the ACP `acp_mcp_servers` computer
/// arm) use exactly the returned value. Bump and render must be atomic:
/// with them split, two overlapping injections for the same identity could
/// interleave as rotate(A)→rotate(B)→mint(A)→mint(B), handing BOTH children
/// a currently-valid latest-generation bearer — the stale child would keep
/// desktop access under standing grants, exactly what rotation exists to
/// revoke. Held across the render, each injection's token is pinned to its
/// OWN bump: whichever rotation lands last is the only one that still
/// verifies. `#[doc(hidden)] pub` for the same sibling-module/
/// cross-crate-test reason as [`computer_session_token`].
#[doc(hidden)]
pub fn rotate_and_mint_computer_session_token(
    thread: i32,
    dir: &str,
    wt: Option<i32>,
) -> MintedBearer {
    let mut generations =
        session_token_generations().lock().unwrap_or_else(|e| e.into_inner());
    let slot = generations.entry((thread, dir.to_string(), wt)).or_insert(0);
    *slot = slot.wrapping_add(1);
    MintedBearer {
        token: render_computer_session_token(thread, dir, wt, *slot),
        guard: MintGuard {
            thread,
            dir: dir.to_string(),
            wt,
            generation: *slot,
            committed: false,
        },
    }
}

/// What [`rotate_and_mint_computer_session_token`] produced: the bearer the
/// child will carry, and a [`MintGuard`] that revokes it unless the caller
/// says a live process took it.
#[doc(hidden)]
pub struct MintedBearer {
    pub token: String,
    pub guard: MintGuard,
}

/// Ownership receipt for one freshly minted bearer: revokes on drop unless
/// [`MintGuard::commit`] is called.
///
/// # The invariant this exists to enforce
///
/// A minted generation is owned by exactly one LIVE thing — a child process, an
/// app-server connection, an ACP route, or the engine that will spawn one — or
/// it is revoked. Between the mint and that owner existing, the bearer is valid
/// and unowned: the identity's DB rows keep `session_is_live` true, so nothing
/// downstream refuses it, and for claude it is already written to an
/// owner-readable config on disk. Anything that returns, fails, or blocks in
/// that window leaves desktop control reachable by an orphaned descendant or
/// any same-uid process that read the config/env/argv.
///
/// Enforced by the type rather than by convention, because convention did not
/// hold here. The unowned-mint paths are not exotic and they do not look alike:
/// a `?` between the mint and the spawn; a connection torn down on a path with
/// no revoke; a constructor that fails before an engine exists to tear down; a
/// mint left straddling a blocking queue wait, unrevokable because the engine
/// lock is held across it. Each is invisible at the site that introduces it —
/// the code reads as if the bearer simply went to a child. Making the revoke
/// the DEFAULT inverts that: a new path has to SAY that something took the
/// bearer, and `#[must_use]` plus the absence of any other way to obtain the
/// generation is what forces it to.
///
/// # Why dropping is safe
///
/// Drop goes through [`revoke_computer_session_token_generation`], which is
/// compare-and-revoke: a guard whose generation was already superseded by
/// another injection for the same identity changes nothing. So a late or
/// duplicate drop can never take a live replacement's bearer with it — which
/// matters because `(thread, dir, wt)` is shared between sessions of one
/// direction and worktree.
///
/// Revocation is a synchronous map bump, so it is safe to do in `Drop`.
///
/// # Drop TIMING is not automatic — the one thing the type cannot enforce
///
/// A guard runs at the end of its SCOPE, not at the moment its owner is decided
/// to be dead. On a path that AWAITS between those two points — tearing a
/// doomed connection down with `shutdown_and_reap`, `cancel`, `unsubscribe` —
/// the bearer stays valid for the whole wait, which is exactly the window a
/// Stop is trying to close. Those paths must `drop(mint)` explicitly, before
/// the await, and say why. The compiler will not ask.
///
/// The rule of thumb: if the code between "this is dead" and the return
/// contains an `.await`, drop first.
#[doc(hidden)]
#[must_use = "an unheld mint is revoked on drop — commit it once a live child               or connection carries the bearer, or drop it deliberately"]
pub struct MintGuard {
    thread: i32,
    dir: String,
    wt: Option<i32>,
    generation: u64,
    committed: bool,
}

impl MintGuard {
    /// A live child/connection now carries this bearer, and its caller records
    /// the returned generation as the owner's. Disarms the drop-revoke.
    pub fn commit(mut self) -> u64 {
        self.committed = true;
        self.generation
    }

    /// The generation this guard would revoke, WITHOUT disarming it. For a
    /// caller that must name the generation while the hand-off is still
    /// pending (an ACP route cell seeded before the consumer exists).
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl Drop for MintGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        revoke_computer_session_token_generation(self.thread, &self.dir, self.wt, self.generation);
    }
}

/// Invalidate the bearer minted at `generation` for `(thread, dir, wt)` WITHOUT
/// minting a replacement — the teardown counterpart of
/// [`rotate_and_mint_computer_session_token`]. Returns whether it revoked.
///
/// Rotation alone happens at INJECTION, which covers every case where a
/// replacement child takes over an identity. It does NOT cover a session
/// that is simply STOPPED and never relaunched: `engine::stop` kills the
/// child but leaves the identity's DB rows intact, so neither the deletion
/// revocation set nor `session_is_live` (which reads rows, not engine
/// liveness) refuses the old bearer, and with no new injection the
/// generation never moves. A process that retained the token — an orphaned
/// descendant, or any same-uid process that read it out of the injected
/// config/env/argv — could therefore keep driving the desktop after the
/// human stopped the session, silently so under a standing Full/Always
/// grant. Bumping the generation here kills that token the instant Stop
/// lands.
///
/// # Why this is compare-and-revoke, not an unconditional bump
///
/// The generation is a property of the IDENTITY, but the bearer is held by a
/// specific CHILD, and an identity outlives any one of them. `(thread, dir,
/// wt)` is `(thread, direction, worktree)` for a worker — two sessions of the
/// same direction in the same worktree share it — so a replacement child's
/// injection can rotate the shared generation while the child it replaced is
/// still winding down. An unconditional bump from that older child's teardown
/// would then invalidate the REPLACEMENT's current bearer, and every computer
/// call from that healthy session would 401. Caller-local guards can't see
/// this: each engine's `generation`/`turn_id` counters are its own, and agree
/// that its own teardown is legitimate.
///
/// Requiring the caller to name the generation it minted turns "revoke this
/// identity" into "revoke MY child's bearer", which is the actual intent
/// everywhere. A caller whose generation has been superseded gets `false` and
/// changes nothing — correctly, because rotation already killed its token.
///
/// Harmless for a generation that was never current (a stale record, or `0`,
/// which no mint ever produces — the first mint lands on 1).
///
/// A caller that revokes MUST also guarantee a fresh injection before the
/// same identity's next child starts, or that child inherits a
/// now-invalid bearer — see `lead_chat::engine::refresh_computer_injection`,
/// which every respawn path runs for exactly this reason.
#[doc(hidden)]
pub fn revoke_computer_session_token_generation(
    thread: i32,
    dir: &str,
    wt: Option<i32>,
    generation: u64,
) -> bool {
    let mut generations =
        session_token_generations().lock().unwrap_or_else(|e| e.into_inner());
    let slot = generations.entry((thread, dir.to_string(), wt)).or_insert(0);
    if *slot != generation {
        return false;
    }
    *slot = slot.wrapping_add(1);
    true
}

/// The ONE place this module's HMAC key material gets constructed — shared by
/// both [`computer_session_token`] (mint) and [`verify_computer_token`]
/// (verify), so the two can never drift onto two different derivations of
/// "the MAC for this (thread, dir)". `None` when the OS RNG could not
/// provide the process secret (see [`computer_endpoint_secret`] — retried on
/// a later call, refused fail-closed on this one) or if `HmacSha256::
/// new_from_slice` itself rejects the key — the latter is a can't-happen
/// path in practice (HMAC accepts a key of ANY length, including this fixed
/// 32-byte CSPRNG buffer); both are matched explicitly (never
/// `.expect()`/`.unwrap()`) per CLAUDE.md's ban on panicking in a
/// production path.
fn computer_token_mac(thread: i32, dir: &str, wt: Option<i32>) -> Option<HmacSha256> {
    computer_token_mac_at(thread, dir, wt, session_token_generation(thread, dir, wt))
}

/// [`computer_token_mac`] with an EXPLICIT generation — the mint side of an
/// atomic rotation renders under the generation its own bump produced (see
/// [`rotate_and_mint_computer_session_token`]), while verification always
/// goes through [`computer_token_mac`]'s current-generation read.
fn computer_token_mac_at(
    thread: i32,
    dir: &str,
    wt: Option<i32>,
    generation: u64,
) -> Option<HmacSha256> {
    let secret = computer_endpoint_secret()?;
    let mut mac = <HmacSha256 as Mac>::new_from_slice(secret).ok()?;
    // the
    // MAC binds the EXACT worktree this URL carries, not just `(thread, dir)`.
    // Sibling worker sessions of one multi-repo direction share a single
    // `(thread, dir)` but differ only by `wt`; binding just `(thread, dir)`
    // gave every one of them the SAME bearer, so any of them could swap its
    // URL's `?wt=` to a sibling's id and route screenshots/audit into that
    // sibling's namespace under its own otherwise-valid token and shared
    // grants. Folding `wt` into the MAC means a swapped `?wt=` no longer matches
    // the token the worker was actually issued. The `none` marker for the
    // absent/lead case (no worktree at all) is a DISTINCT representation that
    // can never collide with any explicit `wt<id>` — a separate lead/absent
    // encoding, never a collapsible one.
    let wt_repr = match wt {
        Some(id) => format!("wt{id}"),
        None => "none".to_string(),
    };
    // The identity's token generation is part of the MAC input: a token
    // minted before the latest rotation for this identity no longer
    // recomputes, so a replaced session's bearer dies at the entry gate —
    // see [`session_token_generations`].
    mac.update(format!("{thread}/{dir}/{wt_repr}/g{generation}").as_bytes());
    Some(mac)
}

/// Render the hex token for one explicit generation — shared by
/// [`computer_session_token`] (current generation) and the atomic
/// [`rotate_and_mint_computer_session_token`] (its own just-bumped one).
fn render_computer_session_token(thread: i32, dir: &str, wt: Option<i32>, generation: u64) -> String {
    match computer_token_mac_at(thread, dir, wt, generation) {
        Some(mac) => hex::encode(mac.finalize().into_bytes()),
        None => "token-mac-unavailable-not-valid-hex".to_string(),
    }
}

/// The per-session token [`inject::computer_url`] appends as `&key=<token>`
///  — hex(HMAC-SHA256(process secret,
/// "{thread}/{dir}")). Deterministic for the SAME `(thread, dir)` (so a
/// worker's own injected URL keeps working for the life of the process — this
/// is never re-minted per-call), but unforgeable without either the process
/// secret (never leaves memory) or reading it back off a legitimate worker's
/// own already-injected config (the same-uid residual — see this module's own
/// top-of-file doc comment).
///
/// `#[doc(hidden)] pub` (not `pub(crate)`): `bus::inject` is a SIBLING module
/// (not a descendant of this one) that mints this into the URL it injects,
/// and `tests/computer_mcp.rs` is a separate integration-test crate that needs
/// to build the SAME token to drive the real endpoint — mirrors `args_digest`/
/// `MAX_TYPE_CHARS`'s own doc comments on why a cross-module/cross-crate
/// test-and-production-shared item is exposed this way. When no MAC can be
/// built (an OS RNG that cannot provide the process secret, or the
/// can't-happen HMAC-construction failure — see [`computer_token_mac`]'s own
/// doc), this returns a fixed sentinel string that is not valid hex and
/// therefore can NEVER equal a legitimately hex-encoded `key=` a caller
/// could ever supply — keeping mint/verify symmetric (both go through the
/// identical fallback) and fail-closed, rather than silently minting (or
/// accepting) an empty/predictable token.
#[doc(hidden)]
pub fn computer_session_token(thread: i32, dir: &str, wt: Option<i32>) -> String {
    render_computer_session_token(thread, dir, wt, session_token_generation(thread, dir, wt))
}

/// Constant-time verification of a caller-supplied `key` against the token
/// [`computer_session_token`] would mint for THIS EXACT path `(thread, dir)`
///  — used by [`handle_computer`] before anything
/// else runs. Goes through `hmac`'s own `Mac::verify_slice` (backed by
/// `subtle`'s constant-time equality) rather than decoding+comparing hex
/// strings with `==`, which would short-circuit on the first mismatching
/// byte and leak timing information about how much of the token the caller
/// got right. A `supplied` that isn't even valid hex fails immediately
/// (`hex::decode` error) — there is no valid token shape it could be
/// mistaken for.
fn verify_computer_token(thread: i32, dir: &str, wt: Option<i32>, supplied: &str) -> bool {
    verify_computer_token_at_current_generation(thread, dir, wt, supplied).is_some()
}

/// [`verify_computer_token`] PLUS the capture the in-flight staleness checks
/// need: on success, returns the exact generation the supplied bearer
/// verified at. Read and verify happen against ONE generation value (read
/// first, MAC'd explicitly at it) — verifying at "current" and then reading
/// "current" again as the capture would race a rotation in between, blessing
/// a stale bearer with the replacement's generation. Every later
/// [`verify_bearer_generation`] checkpoint compares this captured value
/// against the identity's then-current generation.
fn verify_computer_token_at_current_generation(
    thread: i32,
    dir: &str,
    wt: Option<i32>,
    supplied: &str,
) -> Option<u64> {
    let auth_gen = session_token_generation(thread, dir, wt);
    let Ok(supplied_bytes) = hex::decode(supplied) else {
        return None;
    };
    let mac = computer_token_mac_at(thread, dir, wt, auth_gen)?;
    if mac.verify_slice(&supplied_bytes).is_ok() {
        return Some(auth_gen);
    }
    None
}

/// The in-flight bearer-staleness check: `auth_gen` is the generation this
/// request's bearer verified at on entry
/// ([`verify_computer_token_at_current_generation`]); a rotation — a
/// replacement child's injection for the SAME `(thread, dir, wt)` — bumps
/// the identity's current generation, and from that instant the superseded
/// child must stop ACTING, not merely fail its NEXT request's entry gate: a
/// request already inside can sit parked on an approval card, the input
/// flight guard, a capture-semaphore queue, or a blocking-pool queue for
/// far longer than a relaunch takes, and would otherwise still drive the
/// desktop (or consume a human's card answer) under an identity a newer
/// child now owns. Lock-only and synchronous, so it can run at every
/// post-wait checkpoint, including inside blocking-pool closures.
fn verify_bearer_generation(
    thread: i32,
    dir: &str,
    wt: Option<i32>,
    auth_gen: u64,
) -> Result<(), String> {
    if session_token_generation(thread, dir, wt) == auth_gen {
        return Ok(());
    }
    Err(
        "this session's bearer was superseded while the call was in flight (a newer session now \
         owns this identity) — the call was not executed"
            .to_string(),
    )
}

// `thread`/`dir` come from the URL path (same identity-can't-be-spoofed
// guarantee `bus::server::handle` relies on for `/bus/:thread/:dir/mcp`).
//
// `?wt=<worktree_id>`: an OPTIONAL query param a
// caller can attach when it already knows the EXACT worktree its own worker
// session materialized into — see `inject::computer_url`'s doc for who sets
// it and why. Without it, the pre-existing "first worktree for this
// direction" fallback is unchanged — a bare `?wt=`-less URL behaves exactly
// as it always did. A malformed/non-numeric `wt`
// is now REJECTED outright — see [`WtParam`] — rather than silently taking
// that same fallback.
pub async fn handle_computer(
    Path((thread, dir)): Path<(i32, String)>,
    Query(q): Query<HashMap<String, String>>,
    State(db): State<Db>,
    State(asks): State<AskRegistry>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    // reject BEFORE the request's `method`/`id`
    // are even inspected, and before ANY authorization logic (`approve`'s own
    // gate included) ever runs — see this module's own top-of-section doc
    // comment for the full rationale. A caller with no `key=`, an empty one,
    // or one that doesn't match THIS EXACT path's `(thread, dir)` gets a bare
    // 401 — no JSON-RPC envelope, no hint about method/id/tool shape, nothing
    // that would help a guessing caller narrow down the real token.
    //
    // the SAME token is now also
    // accepted as `Authorization: Bearer <token>` — codex carries its MCP
    // config on argv (`-c` flags), where a `?key=` URL would be world-readable
    // through process listings, so its injection names an env var instead and
    // codex's own `bearer_token_env_var` support turns that into this header
    // (see `inject::inject_computer`'s codex arm). One verification path for
    // both channels: whichever the caller supplied is checked against the ONE
    // `(thread, dir, wt)` MAC — a query key, when present, wins (every other
    // engine still uses it), and an empty/absent one falls through to the
    // header.
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let supplied_key = match q.get("key").map(String::as_str) {
        Some(k) if !k.is_empty() => k,
        _ => bearer,
    };
    // the
    // per-session bearer is bound to the EXACT worktree this URL carries, not
    // just `(thread, dir)` — see [`computer_token_mac`]'s own doc for the
    // sibling-worktree hijack this closes. Resolve `?wt=` FIRST: a malformed one
    // can correspond to no minted token, so it fails auth CLOSED here (a bare
    // 401, indistinguishable from any other bad key — it never reveals that the
    // path or a worktree exists), while a well-formed absent/explicit `wt` is
    // exactly what the token is then verified against.
    let wt = match WtParam::parse(&q).resolve() {
        Ok(w) => w,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    // The generation this bearer verified at rides along into the dispatch
    // path: the entry gate only ever 401s NEW requests, so every later
    // long-wait checkpoint re-compares this captured value against the
    // identity's then-current generation (see [`verify_bearer_generation`])
    // to stop a request that outlived the child it authenticated for.
    let Some(auth_gen) =
        verify_computer_token_at_current_generation(thread, &dir, wt, supplied_key)
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    // a still-valid token is
    // refused once its owning thread has been deleted — `delete_thread` revokes
    // the route. Fast path is a lock-only set lookup (live sessions and the
    // synthetic-identity tests pay nothing); only a revoked thread pays one
    // `session_is_live` DB check, which also lets a REUSED thread id back in.
    if computer_routes_revoked(thread) && !session_is_live(&db, thread, &dir, wt).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Notifications (no id) get a bare 202, same as the other bus handlers.
    let id = match req.get("id") {
        Some(v) => v.clone(),
        None => return StatusCode::ACCEPTED.into_response(),
    };
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    let result: Value = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "weft_computer", "version": "1.0.0" }
        }),
        "tools/list" => json!({ "tools": computer_tool_specs() }),
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            call_computer(&db, &asks, thread, &dir, wt, auth_gen, name, &args).await
        }
        _ => json!({}),
    };

    sse(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// The `action`s this server implements. Kept as ONE closed list feeding
/// BOTH the JSON Schema `enum` below and the "unknown action" error text, so
/// the schema can never advertise an action this dispatcher doesn't handle
/// (or silently drift from it) — the same closed-list-as-single-source
/// discipline `EXCLUDED_TERMINAL_APPS` in `computer/mod.rs` uses.
const VALID_ACTIONS: &[&str] = &[
    "screenshot",
    "list_windows",
    "left_click",
    "right_click",
    "double_click",
    "triple_click",
    "type",
    "key",
    "scroll",
    "left_click_drag",
    "mouse_move",
    "cursor_position",
    "wait",
];

/// The ONE rendering of the "unknown action" rejection, shared by BOTH
/// [`pure_validate`] (which now rejects an unrecognized action FIRST, before
/// the approval gate) and [`run_action`]'s own
/// fail-closed `_` arm. These two used to
/// format the message independently — `pure_validate` emitted a bare
/// `unknown action: <a>` while `run_action` listed the valid ones — so once
/// `pure_validate` began winning the race, the caller (and the
/// `computer_mcp` integration test that asserts the valid list is named)
/// stopped seeing the action list at all. Routing both through this single
/// function keeps them from ever drifting again — same
/// closed-list-as-single-source discipline `VALID_ACTIONS` itself follows.
fn unknown_action_error(action: &str) -> String {
    format!("unknown action '{action}'; valid actions: {}", VALID_ACTIONS.join(", "))
}

fn computer_tool_specs() -> Value {
    json!([
        {
            "name": "computer",
            "description": format!("Observe AND control the human's screen — OS-level window listing/screenshot plus mouse/keyboard input injection. `action=list_windows` lists visible on-screen windows (Weft's own window and terminal-emulator apps are excluded, so you can never see yourself or the terminal you're running inside). `action=screenshot` captures ONE window — never the whole desktop — and returns a PNG FILE PATH; for clients that accept an inline MCP image (Claude, and ACP/omp sessions) the same result ALSO carries the screenshot inlined as an image block, so you can reason over it directly with no need to open the path yourself — other clients should open that file path with their own image-viewing tool. Every action except `list_windows`, `cursor_position`, and `wait` needs `window` (same id/substring rule as screenshot) and drives that window's input: `left_click`/`right_click`/`double_click`/`triple_click`/`mouse_move` need `coordinate` [x, y]; `left_click_drag` needs `start_coordinate` AND `coordinate` (end point); `scroll` needs `coordinate` plus `scroll_direction`; `type`/`key` need `text` (literal text to type, or a combo like \"cmd+s\"/\"ctrl+shift+t\"/\"Return\"/\"f5\" for `key`). ALL coordinates are in the pixel space of the MOST RECENT screenshot of that window — you MUST screenshot a window before clicking/dragging/scrolling/moving the mouse in it, or the call is rejected (there is no screenshot on file yet to map coordinates against); a coordinate maps proportionally onto the window's CURRENT position and size even if it moved or resized since that screenshot, and an out-of-range coordinate (judged against the screenshot's own dimensions) is rejected rather than guessed at. `type`/`key` additionally require a `left_click`/`right_click`/`double_click`/`triple_click` on that SAME window within the last {FOCUS_FRESHNESS_SECS}s — click inside the target window first to focus it, then type/key within {FOCUS_FRESHNESS_SECS}s, or the call is rejected. Every call — observation or input — may pause for a human's permission card (Needs-you) before it runs; an input action can additionally come back `Busy` (another session currently has it) or fail while a DIFFERENT permission card is still waiting on the human — retry after a moment. Input actions are also rate-limited to roughly 2 per second. This tool is only ever ACTIVE once the human has turned on \"Computer Use\" in weft's Settings; while it's off, every call here fails with a `disabled` result explaining that — this tool being LISTED does not by itself mean it can be used yet."),
            "inputSchema": { "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": VALID_ACTIONS,
                        "description": "screenshot | list_windows | left_click | right_click | double_click | triple_click | type | key | scroll | left_click_drag | mouse_move | cursor_position | wait" },
                    "window": { "type": "string",
                        "description": "Required (non-empty) for every action except list_windows, cursor_position, and wait: a window id from action=list_windows, or a case-insensitive substring of its app name or title." },
                    "coordinate": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2,
                        "description": "[x, y] in the pixel space of the most recent screenshot of `window`. Required for left_click/right_click/double_click/triple_click/mouse_move/scroll, and as the drag END point for left_click_drag." },
                    "start_coordinate": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2,
                        "description": "[x, y] drag START point, same pixel space as `coordinate`. Required for left_click_drag." },
                    "text": { "type": "string",
                        "description": format!("Literal text to type (action=type), or a key combo like \"cmd+s\" / \"ctrl+shift+t\" / \"Return\" / \"f5\" (action=key). Both actions require a click on the SAME `window` within the last {FOCUS_FRESHNESS_SECS}s first — see this tool's own description.") },
                    "scroll_direction": { "type": "string", "enum": ["up", "down", "left", "right"],
                        "description": "Required for action=scroll." },
                    "scroll_amount": { "type": "integer",
                        "description": "Scroll distance for action=scroll — default 3, capped at 30." },
                    "duration_ms": { "type": "integer",
                        "description": "Milliseconds to sleep for action=wait — capped at 5000." }
                },
                "required": ["action"] }
        }
    ])
}

async fn call_computer(
    db: &Db,
    asks: &AskRegistry,
    thread: i32,
    dir: &str,
    wt: Option<i32>,
    auth_gen: u64,
    name: &str,
    args: &Value,
) -> Value {
    let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let window_query = args.get("window").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut window_id: Option<u32> = None;
    // Set ONLY by the "screenshot" arm of `run_action`, and ONLY when the
    // engine driving `(thread, dir)` is one `engine_accepts_mcp_image` allows
    // — see that function's doc table.
    let mut screenshot_image_b64: Option<String> = None;

    let outcome = run_action(
        db,
        asks,
        thread,
        dir,
        wt,
        auth_gen,
        name,
        &action,
        args,
        &mut window_id,
        &mut screenshot_image_b64,
    )
    .await;
    let outcome_text = match &outcome {
        Ok(text) => text.clone(),
        Err(text) => text.clone(),
    };

    // The DURABLE audit line's own args payload — NOT the same `args` the
    // approval card in `approve` showed the human — see `redact_audit_args`'s
    // doc for why `action == "type"`'s literal keystrokes are redacted here
    //  but never on the card the human approved BEFORE
    // this call ran.
    let audit_args = redact_audit_args(&action, args);

    append_audit(
        db,
        thread,
        dir,
        wt,
        &AuditEntry {
            ts_ms: now_ms(),
            thread,
            dir,
            action: &action,
            window_query: &window_query,
            window_id,
            args: &audit_args,
            outcome: if outcome.is_ok() { "ok" } else { &outcome_text },
        },
    )
    .await;

    // The image block only ever gets set on the Ok path of a "screenshot"
    // call (see the out-param's own doc above), so `outcome.is_ok()` is
    // implied whenever `screenshot_image_b64` is `Some` — but checking both
    // here rather than relying on that invariant keeps this call site correct
    // even if that ever stops being true.
    match (outcome.is_ok(), screenshot_image_b64) {
        (true, Some(image_b64)) => text_and_image_result(outcome_text, image_b64),
        (true, None) => text_result(outcome_text),
        // a failed call
        // carries `isError: true` — see [`error_result`]'s doc.
        (false, _) => error_result(outcome_text),
    }
}

/// Every action's dispatch, `Ok(confirmation text)` or `Err(error text)` —
/// the text reaches the calling agent either way; an `Err` is additionally
/// marked with the MCP `isError: true` flag (see
/// [`error_result`]); the `Result` split also lets [`call_computer`] report
/// an honest `"ok"`-or-not outcome to the audit log without re-deriving it
/// from the text.
async fn run_action(
    db: &Db,
    asks: &AskRegistry,
    thread: i32,
    dir: &str,
    wt: Option<i32>,
    auth_gen: u64,
    name: &str,
    action: &str,
    args: &Value,
    window_id_out: &mut Option<u32>,
    screenshot_image_b64_out: &mut Option<String>,
) -> Result<String, String> {
    // Fail-closed gate BEFORE touching any backend or even validating the
    // tool/action names: every call here either observes or drives the
    // human's actual screen, so an unset/disabled setting blocks EVERYTHING,
    // not just a well-formed request.
    if !computer::enabled(db).await {
        return Err(ComputerError::Disabled.to_string());
    }
    if name != "computer" {
        // NON-ECHOING: the caller-supplied `name` (from `/params/name`) is
        // NEVER interpolated into this error. `call_computer` persists an
        // action's outcome text verbatim into the durable audit log, and the
        // argument-redaction that scrubs secrets from logged params can't
        // touch a value that arrived as the tool NAME — so echoing a
        // secret-bearing name here would leak it straight past redaction into
        // the audit file. This server exposes exactly one tool; a request for
        // any other name is refused without ever repeating what was asked.
        return Err("unknown tool — this endpoint exposes only the 'computer' tool".to_string());
    }
    // run ALL pure,
    // side-effect-free schema validation for THIS action BEFORE the approval
    // gate below ever opens a card. A malformed call (e.g. `left_click` with no
    // `coordinate`) must be rejected outright — never after a human already
    // answered a card for it, which would mint a standing Always/Full grant for
    // a request that then fails its own arm's parse and never runs. The arms
    // below still re-parse (idempotent) and still do the LIVE checks (window
    // resolution, focus freshness, control lease) that can only run post-approval.
    pure_validate(action, args)?;
    // The server-side approval gate — see this
    // module's own doc comment and `approve`'s. Runs for EVERY action,
    // observation or input, before any action-specific argument is even
    // looked at: a standing grant decides silently, otherwise this blocks on
    // a Needs-you card exactly like `bus::server::handle_ask` does for every
    // other tool call in this crate.
    //
    // for a Write (input) action,
    // refuse to even OPEN a new approval card when this EXACT (thread, dir)
    // already has a DIFFERENT one open and unanswered — checked BEFORE
    // `approve` below, not just in each input arm's own later call (which
    // still runs, unchanged, to catch a card that races in DURING the
    // `approve` await itself — see `check_suspended`'s own doc). Without this
    // earlier check, a human sitting on an unrelated open card who answers
    // THIS action's brand-new one with Always/Full would record a standing
    // grant for a call that then never actually runs (the in-arm check
    // rejects it anyway, since the original card is still open) — two cards
    // opened, one of them entirely wasted, and a grant recorded for
    // authorization that was never exercised. Observation actions
    // (`screenshot`/`list_windows`/`cursor_position`) are untouched — they
    // never take the control lease and were never meant to be blocked by an
    // unrelated pending ask; `wait` IS Write-classified (see
    // `GUI_WRITE_ACTIONS`'s own doc: it's part of an input sequence) and so
    // IS now subject to this same pre-approve gate, even though its own arm
    // below never calls `check_suspended` itself.
    if crate::ask::classify_gui_action(action) == crate::ask::RiskLevel::Write {
        check_suspended(asks, thread, dir)?;
    }
    // `approved` is the window identity `approve`
    // itself resolved AUTHORITATIVELY at the moment it authorized this call,
    // for a Write (input) action with a window argument — `None` for every
    // observe action and for `wait`. Every input arm below threads this
    // through its OWN later, fresh `resolve_window` via
    // `verify_approved_target`, right before it activates/injects — see that
    // function's own doc for the "approve one window, dispatch to a
    // different one" gap this closes.
    let approved = approve(asks, thread, dir, wt, action, args).await?;
    // re-check the kill switch AFTER the approval
    // await returns — NOT just once, up top, before that (potentially very
    // long, up to `bus::server::ASK_WAIT`) wait began. A human can hit Stop
    // (disabling the setting, or tripping the emergency-stop latch —  both
    // are exactly what `computer::enabled` reads) WHILE this exact card is
    // still sitting in Needs-you; if a DIFFERENT click then answers that
    // stale card Allow, the check at the top of this function already ran
    // and would never see the disable. Without this second check, that Allow
    // would sail straight through to dispatch below as if the kill switch had
    // never fired. Cheap (one more `enabled` read) and correct: this is the
    // exact race described above, not a hypothetical.
    if !computer::enabled(db).await {
        return Err(ComputerError::Disabled.to_string());
    }
    // ALSO re-check the bearer's generation after the approval await, for
    // the same reason as the kill-switch recheck above: a replacement launch
    // can rotate this identity's token generation while this call's card sat
    // in Needs-you, and the superseded child must not get to act on (or
    // consume) the human's answer — see `verify_bearer_generation`'s doc.
    verify_bearer_generation(thread, dir, wt, auth_gen)?;
    match action {
        "list_windows" => {
            // acquire the SAME semaphore
            // `screenshot` holds during capture (`screenshot_semaphore` — see
            // its own doc, capacity 2) before this call's own OS enumeration
            // — `MAX_OPEN_OBSERVE_ASKS` only bounds how many
            // observe Ask CARDS may sit open waiting on a human; a session
            // already holding a standing Full/Always grant skips that gate
            // entirely and previously had NO cap at all on how many
            // concurrent `list_windows` calls it could fire, each one a
            // synchronous `xcap` enumeration. Sharing the capture semaphore's
            // budget gives the already-authorized path the same hard
            // concurrency ceiling, rather than inventing a second one.
            let _observe_permit = screenshot_semaphore().acquire().await.map_err(|e| e.to_string())?;
            // recheck the
            // kill switch AFTER the permit queue — with both permits held, a
            // standing-granted caller can sit in `acquire().await` across a
            // Stop/disable, and the only `enabled` read it ever passed was
            // `run_action`'s top gate, long before the queue. Same shape as
            // the screenshot arm's own post-semaphore recheck;
            // this is the arm's LAST await before the enumeration is
            // scheduled.
            if !computer::enabled(db).await {
                return Err(ComputerError::Disabled.to_string());
            }
            // re-check the
            // session's liveness after the observe queue too. A thread deleted
            // while this `list_windows` sat waiting for a permit records a route
            // revocation but does NOT flip `enabled`, so without this a
            // standing-granted caller would still enumerate the desktop under a
            // deleted identity — the input arms' `recheck_after_guard` closes
            // the same gap for injection, this closes it for enumeration. Gated
            // on the revocation set: a never-deleted thread pays only the
            // lock-only lookup, never the `session_is_live` DB check.
            if computer_routes_revoked(thread) && !session_is_live(db, thread, dir, wt).await {
                return Err(SESSION_GONE_MSG.to_string());
            }
            let b = backend::backend();
            let dir_owned = dir.to_string();
            // the enumeration itself
            // moves onto tokio's blocking pool — see `on_blocking`'s own doc
            // for why every OS-touching call here now does.
            on_blocking(move || {
                // final
                // synchronous Stop-latch check on the blocking-pool thread, as
                // the first statement immediately before the OS enumeration — a
                // Stop landing while THIS closure sat QUEUED for a blocking
                // thread must fail closed here, exactly as the screenshot/input
                // closures already do via `recheck_stop_and_lease_before_backend`.
                // `list_windows` holds no control lease, so no lease re-verify —
                // but the stop latch AND
                // the direction-precise route revocation both apply: a thread/
                // direction deleted while this closure sat queued does not trip
                // the global latch, and a standing-granted `list_windows` would
                // otherwise still enumerate app names and window titles under a
                // revoked identity — the same boundary the screenshot and
                // cursor closures already recheck.
                if computer::stop_latched() {
                    return Err(ComputerError::Disabled.to_string());
                }
                if route_revoked_sync(thread, &dir_owned, wt) {
                    return Err(SESSION_GONE_MSG.to_string());
                }
                // and the bearer-staleness check — a rotation landing while
                // this closure sat queued must stop the enumeration too.
                verify_bearer_generation(thread, &dir_owned, wt, auth_gen)?;
                computer::visible_windows(b.as_ref()).map_err(|e| e.to_string())
            })
            .await?
            .map(|windows| serde_json::to_string(&windows).unwrap_or_else(|_| "[]".into()))
        }
        "screenshot" => {
            let window_query = required_window(args)?;
            // verify the window `approve` bound at
            // authorization time (screenshot now binds one too — see that
            // function's own `resolved`/doc comment) is STILL the window
            // about to be captured — fail-closed otherwise, never silently
            // capturing whatever the query happens to resolve to NOW. The
            // read-only twin of every input arm's own
            // `resolve_and_verify_target` gate,
            // applied here to an OBSERVE action for the first time: a card
            // can sit open for a long time (up to `bus::server::ASK_WAIT`)
            // before a human answers it, during which the ORIGINAL window
            // could close and a same-titled replacement take its place — an
            // exact-Always grant has the identical gap between "the window
            // it was granted for" and "whatever the query resolves to on
            // THIS call". Also records `window_id_out` for the audit line,
            // even on the failure path (matches every input arm's own
            // ordering — see that helper's doc).
            // First check: fail fast (and still record `window_id_out` for
            // the audit line — `resolve_and_verify_target`'s own doc) before
            // ever awaiting `screenshot_out_dir`/the capture semaphore below
            // for a call that was always going to be rejected anyway. Its
            // OWN `WindowInfo` is intentionally unused past this point —
            // the capture path re-resolves and re-verifies a SECOND
            // time, right before the capture, and THAT result is what gets
            // used for the actual capture/record below.
            // this first
            // (fail-fast) resolve now runs on the blocking pool too — one
            // single, argument-gated enumeration, but still a synchronous
            // `xcap` call that must never run inline on an async worker where
            // concurrent captures could collectively starve the Stop/Escape
            // kill-switch tasks ("just one enumeration" inline is still a
            // residual runtime-occupancy
            // gap — the same reason `approve`'s own authorization-time resolve
            // moved off the runtime). The SECOND resolve below, sitting
            // immediately next to the actual capture, already runs inside this
            // arm's single capture `on_blocking` closure.
            let _ = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            let out_dir = screenshot_out_dir(db, thread, dir, wt).await?;
            // acquire the process-wide capture
            // semaphore BEFORE the synchronous capture below — see
            // `screenshot_semaphore`'s own doc for why (a Full/Always-granted
            // worker could otherwise fire arbitrarily many concurrent
            // `screenshot` calls, each holding its own full-resolution RGBA
            // buffer plus PNG/JPEG encode buffers at once, with no cap
            // anywhere on this path). Held across capture AND every encode
            // this call does; dropped when this arm's block ends (including
            // on an early `?` return from the capture itself).
            let _capture_permit = screenshot_semaphore().acquire().await.map_err(|e| e.to_string())?;
            // whether this call's owning
            // engine accepts an inline MCP image is looked up now, on the
            // runtime — `engine_accepts_mcp_image` awaits the db, but
            // everything past the recheck below (the second resolve through
            // every encode/record) runs inside a SINGLE `on_blocking` closure
            // (see that helper's own doc), and a blocking-pool closure can't
            // itself `.await` anything. The bool crosses that boundary as a
            // plain owned value instead of the lookup itself moving in.
            //
            // this lookup
            // sits BEFORE the enabled recheck below, not after it — the earlier
            // refactor briefly had it after, which re-opened the exact gap the
            // recheck exists to close: a Stop landing while THIS db await was
            // in flight went unseen, and the capture was scheduled anyway. The
            // recheck must be the arm's genuinely LAST await.
            let want_mcp_image = engine_accepts_mcp_image(db, thread, dir).await;
            // recheck session
            // liveness after the capture queue too. The list_windows arm runs
            // the same recheck; `enabled` alone below is not enough. A thread deleted while this call waited for a
            // capture permit records a route revocation but does NOT flip
            // `enabled`, so without this a standing-granted caller would capture
            // pixels after its session is gone — and `screenshot_window` writing
            // into the already-resolved `out_dir` would RECREATE the just-cleaned
            // per-thread output tree. Gated on the revocation set (a never-deleted
            // thread pays only the lock-only lookup, never the DB check).
            if computer_routes_revoked(thread) && !session_is_live(db, thread, dir, wt).await {
                return Err(SESSION_GONE_MSG.to_string());
            }
            // recheck the
            // kill switch AFTER every await this arm took since the last
            // `enabled` check up top — `screenshot_out_dir`, the capture
            // semaphore's own `acquire().await` (which can queue arbitrarily
            // long when both `SCREENSHOT_CONCURRENCY` permits are already
            // held), and the engine lookup just above. Without this, a human
            // hitting Stop / disabling Computer Use while this call sat queued
            // would still capture once a permit freed — a Full/Always-granted
            // caller could leave many captures queued to fire AFTER Stop.
            // This is the arm's LAST `.await` (see the lookup's comment
            // above for that invariant): nothing runs
            // between it and scheduling the blocking capture below.
            if !computer::enabled(db).await {
                return Err(ComputerError::Disabled.to_string());
            }
            // and the bearer-staleness check after the same queueing awaits
            // — a capture must never fire for a child a relaunch's rotation
            // already superseded (see `verify_bearer_generation`).
            verify_bearer_generation(thread, dir, wt, auth_gen)?;
            // re-resolve + re-verify identity
            // AGAIN, after EVERY await this arm takes since the first check
            // above (`screenshot_out_dir`, the capture semaphore itself, then
            // the engine lookup just above) — with BOTH `SCREENSHOT_
            // CONCURRENCY` permits already held by concurrent captures, the
            // `acquire().await` above can queue for arbitrarily long.
            // `screenshot_window` below only re-resolves `window_query` for
            // ITS OWN capture; it does not compare against `approved` at all.
            // Without this second check, the ORIGINAL window could close
            // during that queueing gap and a same-query REPLACEMENT window
            // take its place, and `screenshot_window`'s internal re-resolve
            // would then silently capture the REPLACEMENT's pixels under an
            // approval that was only ever shown for the original — the exact
            // gap this re-verify exists to close, which the capture semaphore's
            // own queueing would otherwise reopen.
            //
            // this resolve, the capture,
            // and every encode/record below now all run INSIDE one
            // `on_blocking` closure — moved off the async runtime for the
            // same reason `on_blocking`'s own doc gives (a slow/queued OS
            // call here must never risk starving the Stop/Escape kill
            // switch). The "no further `.await` happens between this check
            // and the capture" guarantee this comment used to make still
            // holds in spirit: the closure itself contains no `.await` at all
            // (a plain sync closure — `spawn_blocking` requires that), and
            // the resolve inside it runs immediately before the capture, on
            // the SAME blocking-pool thread, with nothing else able to run in
            // between — so the "just verified" identity guarantee
            // is preserved exactly, just now separated from this check
            // by a `spawn_blocking` scheduling boundary instead of sitting in
            // the same async stack frame. `resolve_and_verify_target` takes
            // `window_id_out` as `&mut Option<u32>` — a reference into THIS
            // function's own stack frame, which can't cross into a `'static`
            // blocking closure — so the closure below runs it against a
            // throwaway LOCAL `Option<u32>` instead and returns that
            // alongside its own result; written back into the real
            // `window_id_out` immediately after `on_blocking` returns (below),
            // preserving `resolve_and_verify_target`'s existing "record the
            // id even when identity verification fails" semantics exactly.
            let window_query_owned = window_query.to_string();
            let approved_for_capture = approved.clone();
            let dir_owned = dir.to_string();
            // Kept for the earlier post-write recheck below — `out_dir`
            // itself moves into the capture closure.
            let out_dir_cleanup = out_dir.clone();
            let b = backend::backend();
            let (resolved_id, capture) = on_blocking(move || {
                let mut resolved_id: Option<u32> = None;
                let w = match resolve_and_verify_target(
                    &window_query_owned,
                    &approved_for_capture,
                    &mut resolved_id,
                ) {
                    Ok(w) => w,
                    Err(e) => return (resolved_id, Err(e)),
                };
                // capture
                // the EXACT window `w` just verified — `computer::screenshot_
                // resolved` takes the already-resolved `WindowInfo` instead of
                // the raw query, so there is NO third enumeration left for a
                // same-query replacement to slip into between verify and
                // capture (the old `screenshot_window(query, ..)` call
                // re-resolved internally; two OS enumerations back-to-back
                // are still two). If `w` closed in the instant since the
                // verify above, `capture_window(w.id)` fails closed rather
                // than ever falling back to a lookalike.
                // recheck
                // the stop latch on THIS blocking thread, immediately before
                // the capture. The arm's last `enabled` recheck ran BEFORE this
                // closure was scheduled; a Stop landing while the closure sat
                // queued for a blocking thread — or while the final resolve
                // just above stalled — would otherwise let the capture proceed
                // AFTER Emergency Stop. `screenshot` holds no control lease
                // (it is ReadOnly), so only the stop latch is checked here (the
                // input arms' `recheck_stop_and_lease_before_backend` also
                // re-checks the lease).
                if computer::stop_latched() {
                    return (resolved_id, Err(ComputerError::Disabled.to_string()));
                }
                // the async
                // liveness recheck ran BEFORE this closure was scheduled — and
                // before the `enabled` await and the blocking-pool queue wait
                // that follow it. A delete landing in that window can't be seen
                // by an async `session_is_live` here (this is a SYNC blocking-
                // pool closure), but the direction-precise revocation flag CAN be
                // — the delete paths publish it before their cascade. Fail closed
                // so a queued capture can't write pixels under the deleted
                // identity or recreate the just-pruned output subtree. Direction-
                // precise, so a `delete_repo` that left THIS direction live (only
                // a sibling was removed) is unaffected.
                if route_revoked_sync(thread, &dir_owned, wt) {
                    return (resolved_id, Err(SESSION_GONE_MSG.to_string()));
                }
                // and the bearer-staleness check on this same blocking
                // thread — a rotation landing while this closure sat queued
                // must stop the capture too (see `verify_bearer_generation`).
                if let Err(e) = verify_bearer_generation(thread, &dir_owned, wt, auth_gen) {
                    return (resolved_id, Err(e));
                }
                let shot = match computer::screenshot_resolved(b.as_ref(), &w, &out_dir) {
                    Ok(s) => s,
                    Err(e) => return (resolved_id, Err(e.to_string())),
                };
                // this confirmation text is shared
                // by BOTH the plain-text-only result and `text_and_image_
                // result`'s own text block (see that function's doc) —
                // worded so it holds for either: a capable client (Claude,
                // ACP/omp) also gets this screenshot inlined as an image
                // block in the SAME result and can reason over it directly;
                // any other client opens the path.
                let text = format!(
                    "screenshot saved: {} ({}x{}, scale {:.2}) — inlined as an image in this result for \
                     capable clients (Claude, ACP/omp), otherwise open the path with your own image \
                     viewing tool",
                    shot.path.display(),
                    shot.width,
                    shot.height,
                    shot.scale
                );
                // Both the preview registry and the MCP image block need the
                // raw pixels — read straight off `shot.pixels`, the SAME
                // in-memory RGBA `screenshot_window` itself scaled and saved —
                // never re-opened from `shot.path`.
                // This used to re-read the just-saved PNG back off disk
                // (`read_captured_image`, now deleted): a worker-writable
                // `out_dir` is repository-controlled content, and the gap
                // between that PNG's own save and this re-open was an open
                // TOCTOU/symlink window — a sandboxed background process
                // could swap the freshly-saved file for a symlink to an
                // arbitrary user-readable image in that gap, and Weft would
                // follow it with its own permissions, inlining the
                // substituted pixels to the model/human as if they were the
                // real capture. Reading `shot.pixels` instead closes that
                // window entirely rather than narrowing it — there is no
                // second filesystem access left to race. `encode_jpeg_data_
                // uri` can still fail on `shot.pixels` (encoding, not
                // decoding, can theoretically error) — best-effort, same as
                // before: a failure here must never fail a screenshot that
                // already saved successfully, it just means no
                // preview/image this call.
                //
                // Keyed to `shot.window_id` — the id `computer::
                // screenshot_window` ITSELF already resolved and captured
                // against — rather than
                // re-resolving `window_query` a second time here: a second
                // resolution can land on a DIFFERENT window than the one
                // actually captured if it closed, was renamed, or its id got
                // reused in the gap between the two calls, silently
                // mis-keying the preview (and any input approval card that
                // later attaches it) to the WRONG window. `computer::
                // Screenshot::window_id` closes that gap by construction —
                // there is no second resolution left to drift.
                if let Ok(preview) =
                    computer::encode_jpeg_data_uri(&shot.pixels, PREVIEW_LONG_EDGE, PREVIEW_QUALITY)
                {
                    // Store the FULL window identity (id + app + title) this
                    // capture came from, not the numeric id alone — `w` is
                    // the already-resolved, just-verified target (its `id`
                    // equals `shot.window_id` by that verification), so this
                    // reuses it rather than re-resolving. See
                    // [`VerifiedWindowIdentity`] for the id-reuse hazard this
                    // closes.
                    store_screenshot_preview(thread, &dir_owned, wt, preview, VerifiedWindowIdentity::from_window(&w));
                }
                // record THIS capture's own saved
                // dimensions for (thread, dir, shot.window_id) — every
                // coordinate-taking input arm below maps against whatever is
                // on file here (see `computer::map_screenshot_coord`'s own
                // doc), fail-closed if nothing is. Recorded unconditionally,
                // on every successful capture, regardless of which engine is
                // asking — matches `store_screenshot_preview`'s own "refresh
                // every successful screenshot" rule right above.
                computer::record_shot_dims(thread, &dir_owned, wt, shot.window_id, shot.width, shot.height, &w);
                // The MCP `image` content block is engine-gated — see
                // `engine_accepts_mcp_image`'s doc table; `want_mcp_image`
                // was already decided on the runtime, above, before this
                // closure started (see that change-16 P1 comment).
                let image_b64 = if want_mcp_image {
                    computer::encode_jpeg_data_uri(&shot.pixels, MCP_IMAGE_LONG_EDGE, MCP_IMAGE_QUALITY)
                        .ok()
                        .and_then(|uri| strip_data_uri_prefix(&uri).map(str::to_string))
                } else {
                    None
                };
                (resolved_id, Ok((text, image_b64)))
            })
            .await?;
            *window_id_out = resolved_id;
            // re-check the
            // route revocation AFTER the capture/save, under the SAME
            // [`revocation_txn_lock`] the delete flows hold from before they
            // publish revocation until after their output removal — the
            // closure's own pre-capture check is check-then-act across the
            // whole blocking capture, so a delete could interleave and the
            // save's `create_dir_all(out_dir)` would recreate the deleted
            // session's subtree (the same gap `append_audit` closes, here on
            // the screenshot write path). Two outcomes only:
            // either this check (which waits out any in-flight delete
            // transaction) sees the revocation — then THIS call's own
            // recreated output is removed and the call fails with the same
            // SESSION_GONE the pre-capture check returns — or the delete
            // starts after this check released the lock, and its own removal
            // then sweeps the file with the rest of the tree. The lock is
            // NOT held across the capture itself: a multi-hundred-ms OS
            // capture must never stall every delete and audit append behind
            // a global mutex, so the recheck runs after, paying cleanup only
            // in the raced case. Runs before `capture?` so a failed capture
            // that already created directories is cleaned up the same way.
            {
                let _revocation_txn = revocation_txn_lock().lock().await;
                if route_revoked_sync(thread, dir, wt) {
                    remove_recreated_screenshot_output(&out_dir_cleanup);
                    return Err(SESSION_GONE_MSG.to_string());
                }
            }
            let (text, image_b64) = capture?;
            if let Some(b64) = image_b64 {
                *screenshot_image_b64_out = Some(b64);
            }
            Ok(text)
        }
        "left_click" | "right_click" | "double_click" | "triple_click" => {
            // every PURELY-argument-shaped check
            // for this action (the window argument being non-empty, the
            // coordinate's shape) runs BEFORE the control lease/throttle are
            // touched — see this section's own "input gates" doc comment
            // further down for the full ordering rationale. The
            // window's actual RESOLUTION and
            // the coordinate's mapping against it are NOT purely-argument
            // checks (they depend on the live desktop's current state) and
            // now run AFTER `input_flight_guard`/the first
            // `recheck_after_guard` instead — see the comment right below.
            let window_query = required_window(args)?;
            let (cx, cy) = parse_coordinate(args, "coordinate")?;
            check_suspended(asks, thread, dir)?;
            let button = if action == "right_click" { MouseButton::Right } else { MouseButton::Left };
            let count: u32 = match action {
                "double_click" => 2,
                "triple_click" => 3,
                _ => 1,
            };
            acquire_and_throttle(thread, dir, wt)?;
            // Held for the FULL duration of the backend call, per the
            // cross-module contract on `computer::input_flight_guard`'s own
            // doc — a second `tools/call` for the SAME (thread, dir) racing
            // in concurrently must serialize here rather than interleave its
            // own click on the human's real desktop.
            let _flight = computer::input_flight_guard().await;
            recheck_after_guard(db, asks, thread, dir, wt, auth_gen).await?;
            // resolve the window AND map
            // the coordinate FRESH here, after the flight guard — not
            // before it, as this used to. A call that queued on the guard
            // behind another session's in-flight action could sit there for
            // as long as that action's own backend round trip takes; a
            // window resolved/mapped BEFORE the guard could have since
            // moved, resized, closed, or had its id reused by the time this
            // call actually gets to inject, landing the click outside the
            // (now stale) coordinates were computed against — or on an
            // entirely different window that reused the same id.
            //
            // this window must still be the EXACT
            // one `approve` bound at authorization time — see
            // `verify_approved_target`'s own doc. Checked BEFORE this window
            // is ever activated or clicked (`resolve_and_verify_target`
            // does both: resolve, record `window_id_out`, verify).
            let w = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            // PREFLIGHT
            // the coordinate mapping against the pre-activation window — a
            // call whose prerequisite is already missing (no recorded
            // screenshot for this window, or an out-of-bounds coordinate)
            // must fail BEFORE activation raises and focuses the target
            // application, not after that side effect already stole the
            // foreground. Advisory only: the AUTHORITATIVE mapping still runs
            // against the post-activation `w2` below, unchanged (freshness —
            // the window can move/resize during the blocking activation).
            let _ = map_input_coord(thread, dir, wt, &w, cx, cy)?;
            // Everything TARGET-facing from here on — reclaiming the
            // foreground (see `activate_target`'s own doc for why even the
            // click family needs that, UNCONDITIONALLY, Auto approvals
            // included: an Interactive approval card can cover the target's
            // real on-screen position, and an Auto approval offers no
            // guarantee the target still holds the real OS foreground), the
            // authoritative fresh re-resolve/re-verify, the coordinate
            // mapping, and the click itself — runs inside ONE paced blocking
            // closure, back-to-back with the backend call: see
            // `pace_activate_verify_and_inject`'s own doc for why no await/
            // queue/sleep gap may separate any of those steps from the
            // injection. `dir` and the backend handle cross into the
            // `'static` closure as owned values.
            let dir_owned = dir.to_string();
            let b = backend::backend();
            let ((px, py), w2) = pace_activate_verify_and_inject(
                asks,
                thread,
                dir,
                wt,
                auth_gen,
                window_query,
                &approved,
                w,
                window_id_out,
                move |fresh| {
                    let (px, py) = map_input_coord(thread, &dir_owned, wt, fresh, cx, cy)?;
                    b.click(px, py, button, count).map_err(|e| e.to_string())?;
                    Ok((px, py))
                },
            )
            .await?;
            // A click that actually reached the OS is presumed to have
            // handed this window OS focus — see `recent_clicks`'s doc. Only
            // AFTER the backend call succeeds: a rejected/failed click never
            // touched the real window and must not seed a false freshness
            // record for a later `type`/`key`.
            record_click_focus(thread, dir, wt, &w2);
            Ok(format!(
                "{action} at ({px}, {py}) in window {} done — take a screenshot to verify",
                w2.id
            ))
        }
        "mouse_move" => {
            let window_query = required_window(args)?;
            let (cx, cy) = parse_coordinate(args, "coordinate")?;
            check_suspended(asks, thread, dir)?;
            acquire_and_throttle(thread, dir, wt)?;
            // See the click-family arm above for why this guard is held
            // across the backend call itself, and why window resolution/
            // coordinate mapping happen AFTER it.
            let _flight = computer::input_flight_guard().await;
            recheck_after_guard(db, asks, thread, dir, wt, auth_gen).await?;
            let w = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            // preflight before activation — see the
            // click-family arm above.
            let _ = map_input_coord(thread, dir, wt, &w, cx, cy)?;
            // one paced closure for activation, the fresh resolve, mapping,
            // and the injection — see the click-family arm above.
            let dir_owned = dir.to_string();
            let b = backend::backend();
            let ((px, py), w2) = pace_activate_verify_and_inject(
                asks,
                thread,
                dir,
                wt,
                auth_gen,
                window_query,
                &approved,
                w,
                window_id_out,
                move |fresh| {
                    let (px, py) = map_input_coord(thread, &dir_owned, wt, fresh, cx, cy)?;
                    b.move_cursor(px, py).map_err(|e| e.to_string())?;
                    Ok((px, py))
                },
            )
            .await?;
            Ok(format!(
                "mouse_move to ({px}, {py}) in window {} done — take a screenshot to verify",
                w2.id
            ))
        }
        "left_click_drag" => {
            let window_query = required_window(args)?;
            let (sx, sy) = parse_coordinate(args, "start_coordinate")?;
            let (ex, ey) = parse_coordinate(args, "coordinate")?;
            check_suspended(asks, thread, dir)?;
            acquire_and_throttle(thread, dir, wt)?;
            let _flight = computer::input_flight_guard().await;
            recheck_after_guard(db, asks, thread, dir, wt, auth_gen).await?;
            // BOTH endpoints are mapped against the SAME freshly-resolved
            // window — a drag has two coordinates, but only one window to go
            // stale — and that authoritative resolve happens inside the paced
            // closure, right before the injection (see the click-family arm
            // above).
            let w = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            // preflight BOTH endpoints before
            // activation — see the click-family arm above.
            let _ = map_input_coord(thread, dir, wt, &w, sx, sy)?;
            let _ = map_input_coord(thread, dir, wt, &w, ex, ey)?;
            let dir_owned = dir.to_string();
            let b = backend::backend();
            let ((from, to), w2) = pace_activate_verify_and_inject(
                asks,
                thread,
                dir,
                wt,
                auth_gen,
                window_query,
                &approved,
                w,
                window_id_out,
                move |fresh| {
                    let from = map_input_coord(thread, &dir_owned, wt, fresh, sx, sy)?;
                    let to = map_input_coord(thread, &dir_owned, wt, fresh, ex, ey)?;
                    b.drag(from, to).map_err(|e| e.to_string())?;
                    Ok((from, to))
                },
            )
            .await?;
            Ok(format!(
                "left_click_drag from ({}, {}) to ({}, {}) in window {} done — take a screenshot to verify",
                from.0, from.1, to.0, to.1, w2.id
            ))
        }
        "scroll" => {
            let window_query = required_window(args)?;
            let (cx, cy) = parse_coordinate(args, "coordinate")?;
            let (dx, dy) = parse_scroll(args)?;
            check_suspended(asks, thread, dir)?;
            acquire_and_throttle(thread, dir, wt)?;
            let _flight = computer::input_flight_guard().await;
            recheck_after_guard(db, asks, thread, dir, wt, auth_gen).await?;
            let w = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            // preflight before activation — see the
            // click-family arm above.
            let _ = map_input_coord(thread, dir, wt, &w, cx, cy)?;
            // one paced closure for activation, the fresh resolve, mapping,
            // and the injection — see the click-family arm above.
            let dir_owned = dir.to_string();
            let b = backend::backend();
            let ((px, py), w2) = pace_activate_verify_and_inject(
                asks,
                thread,
                dir,
                wt,
                auth_gen,
                window_query,
                &approved,
                w,
                window_id_out,
                move |fresh| {
                    let (px, py) = map_input_coord(thread, &dir_owned, wt, fresh, cx, cy)?;
                    b.scroll(px, py, dx, dy).map_err(|e| e.to_string())?;
                    Ok((px, py))
                },
            )
            .await?;
            Ok(format!(
                "scroll at ({px}, {py}) dx={dx} dy={dy} in window {} done — take a screenshot to verify",
                w2.id
            ))
        }
        "type" => {
            let window_query = required_window(args)?;
            let text = required_text(args)?;
            // a hard length ceiling, checked
            // right after `required_text` and well before any lease/throttle
            // is touched — see `check_type_length`'s own doc for why.
            check_type_length(text)?;
            check_suspended(asks, thread, dir)?;
            acquire_and_throttle(thread, dir, wt)?;
            let _flight = computer::input_flight_guard().await;
            recheck_after_guard(db, asks, thread, dir, wt, auth_gen).await?;
            // Resolve the window (and,
            // right below, check focus-freshness against it) AFTER the
            // flight guard now too — a queued `type` used to resolve the
            // window BEFORE the guard, so a stale id from a closed/reused
            // window could reach `require_recent_focus`/`activate_target`
            // for the wrong target. Focus-freshness gate itself
            // is unchanged in SPIRIT — see
            // `require_recent_focus`'s doc — just now checked against a
            // freshly-resolved id rather than a possibly-stale one.
            let w = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            // PREFLIGHT
            // the focus-freshness prerequisite against the pre-activation
            // window — a `type` with no recent click on this window is doomed
            // anyway, so it must fail BEFORE activation raises and focuses
            // the target application. Advisory only; the AUTHORITATIVE check
            // against the post-activation `w2` below is unchanged.
            require_recent_focus(thread, dir, wt, &w)?;
            let char_count = text.chars().count();
            // `text_owned` crosses into the blocking closure since `text`
            // itself is borrowed from `args`, not `'static`. The
            // AUTHORITATIVE focus-freshness check runs inside the paced
            // closure against the fresh post-activation resolve — the window
            // it guards is the SAME one about to receive the keystrokes —
            // right before the injection (see the click-family arm above).
            let text_owned = text.to_string();
            let dir_owned = dir.to_string();
            let b = backend::backend();
            let (_, w2) = pace_activate_verify_and_inject(
                asks,
                thread,
                dir,
                wt,
                auth_gen,
                window_query,
                &approved,
                w,
                window_id_out,
                move |fresh| {
                    require_recent_focus(thread, &dir_owned, wt, fresh)?;
                    b.type_text(&text_owned).map_err(|e| e.to_string())
                },
            )
            .await?;
            Ok(format!(
                "typed {char_count} char(s) in window {} done — take a screenshot to verify",
                w2.id
            ))
        }
        "key" => {
            let window_query = required_window(args)?;
            let combo = required_text(args)?;
            // validate the combo's
            // SHAPE — a pure, argument-only check, no lease/throttle/backend
            // touched — before `check_suspended`/`acquire_and_throttle`
            // below, mirroring the "purely-argument checks run
            // first" discipline every other input arm already follows (see
            // this section's own "input gates" doc comment further down).
            // Before this, a malformed combo (e.g. "ctrl+a+b") wasn't
            // rejected until `OsBackend::key` actually ran it through
            // `parse_key_combo` a SECOND time — by which point this call had
            // already taken the 30s control lease, consumed a throttle slot,
            // and activated the target window, for a call that was always
            // going to fail. The parsed tokens are discarded here (`let _`)
            // — this exists ONLY to reject a malformed shape early; the
            // combo's ACTUAL injection still goes through `b.key(combo)`
            // below (now via `on_blocking` — see
            // that helper's own doc — but still the SAME backend call,
            // unchanged; os.rs itself is not touched here).
            let _ = computer::parse_key_combo(combo).map_err(|e| e.to_string())?;
            check_suspended(asks, thread, dir)?;
            acquire_and_throttle(thread, dir, wt)?;
            let _flight = computer::input_flight_guard().await;
            recheck_after_guard(db, asks, thread, dir, wt, auth_gen).await?;
            // See the matching comment in the "type" arm above.
            let w = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            // preflight the focus prerequisite before
            // activation — see the "type" arm above.
            require_recent_focus(thread, dir, wt, &w)?;
            let combo_owned = combo.to_string();
            // one paced closure for activation, the fresh resolve, the
            // authoritative focus check, and the injection — see the "type"
            // arm above.
            let dir_owned = dir.to_string();
            let b = backend::backend();
            let (_, w2) = pace_activate_verify_and_inject(
                asks,
                thread,
                dir,
                wt,
                auth_gen,
                window_query,
                &approved,
                w,
                window_id_out,
                move |fresh| {
                    require_recent_focus(thread, &dir_owned, wt, fresh)?;
                    b.key(&combo_owned).map_err(|e| e.to_string())
                },
            )
            .await?;
            Ok(format!(
                "key {combo} in window {} done — take a screenshot to verify",
                w2.id
            ))
        }
        // No window, no control lock, no throttle — this reads the cursor's
        // current position without touching input devices.
        "cursor_position" => {
            // even this near-instant OS
            // call moves onto tokio's blocking pool (see `on_blocking`'s own
            // doc) so it never risks parking an async worker Stop/Escape
            // needs — cheap enough that it doesn't need `screenshot_
            // semaphore`'s concurrency budget (see that constant's own doc
            // for what THAT protects against: buffered capture memory, not a
            // quick position query).
            let b = backend::backend();
            let dir_owned = dir.to_string();
            on_blocking(move || {
                // recheck
                // the Stop latch on THIS blocking thread, immediately before the
                // OS read. The post-approval `enabled` check ran BEFORE this
                // closure was scheduled; a Stop landing while it sat queued would
                // otherwise still return desktop state.
                if computer::stop_latched() {
                    return Err(ComputerError::Disabled.to_string());
                }
                // ALSO
                // recheck the direction-precise route revocation here, exactly
                // like the screenshot capture closure — a thread/direction
                // deleted while this closure sat queued does NOT trip the
                // global stop latch, so a standing-granted `cursor_position`
                // would otherwise still read live desktop cursor state under a
                // revoked identity.
                if route_revoked_sync(thread, &dir_owned, wt) {
                    return Err(SESSION_GONE_MSG.to_string());
                }
                // and the bearer-staleness check — same rationale as the
                // list_windows closure's.
                verify_bearer_generation(thread, &dir_owned, wt, auth_gen)?;
                b.cursor_position().map_err(|e| e.to_string())
            })
            .await?
            .map(|(x, y)| format!("cursor at ({x}, {y})"))
        }
        // No window, no control lock, no throttle — a pure timer.
        "wait" => {
            let ms = parse_duration_ms(args)?;
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            Ok(format!("waited {ms}ms"))
        }
        // Fail-closed, not fail-open: an unrecognized/missing action is
        // rejected with the valid list, never silently treated as one of
        // the known ones. In practice [`pure_validate`] already rejected an
        // unknown action before dispatch ever reached here
        // — this arm stays as defense-in-depth, sharing the
        // SAME [`unknown_action_error`] rendering so the two can't diverge.
        _ => Err(unknown_action_error(action)),
    }
}

// —— server-side approval gate ——

/// how many OBSERVE-class
/// (`RiskLevel::ReadOnly`) computer-use Ask cards may sit open at once for one
/// `(thread, dir)` before further grant-less observe calls fail closed instead
/// of opening more — see `approve`'s observe branch. Small on purpose: a human
/// can only meaningfully consider a couple of screenshot/list_windows cards at
/// a time, and each open card is a held waiter (`bus::server::ASK_WAIT` — up
/// to an hour) plus an IM-bridge push; Write (input) actions are stricter
/// still (ANY open ask suspends them — `check_suspended` /
/// `request_with_preview_unless_open`).
const MAX_OPEN_OBSERVE_ASKS: usize = 3;

/// Gate EVERY `tools/call` here, server-side, before [`run_action`]'s dispatch
/// looks at any action-specific argument — see this module's own top doc
/// comment for the full architecture rationale (this closes the "any local
/// process can just POST past the engine's hook" gap).
///
/// Mirrors `bus::server::handle_ask`'s own shape closely on purpose, so the
/// two Ask-bridge entry points behave identically wherever they overlap:
///  1. Classify (`risk` via [`crate::ask::classify_gui_action`]), build the
///     display `summary` and the canonical `action_key` — `["gui", action,
///     window, args_digest]` (the trailing
///     digest is deliberate — see [`args_digest`]'s own doc), a namespace of its
///     own (distinct from `bus::server::summarize`'s `["mcp", tool_name,
///     args]` for the SAME tool, since this is a different, more precise
///     identity: it never depends on which engine-specific tool-name shape
///     happened to reach this endpoint). `detail` is the FULL, unredacted
///     params JSON — see the inline comment below for why that must never be
///     redacted here even though the PERSISTED audit line
///     (`redact_audit_args`) is.
///  2. A standing grant (`dangerous` mode / Full / Always for this EXACT
///     `action_key`) decides silently via [`crate::ask::AskRegistry::
///     auto_decision_exact`] — unlike
///     `handle_ask`'s own `auto_decision` call, a GUI action is deliberately
///     NEVER swept in by the coarse read-only batch-or-issue grant
///     — see that method's own doc for why a `screenshot`/`list_windows`
///     call (always `RiskLevel::ReadOnly`, regardless of which window it
///     targets) cannot be treated the same as an ordinary read-only tool
///     call for this purpose.
///  3. Otherwise, a Needs-you card ([`crate::ask::AskRegistry::
///     request_with_preview`], `tool` = the literal `"computer"` — this
///     endpoint has no engine identity of its own the way a hook's `?tool=`
///     query does) blocks until answered, with `bus::server::ASK_WAIT`'s own
///     ceiling and the SAME fail-closed deny `handle_ask` returns on a
///     timeout or a cancelled ask (never an ambiguous default).
///
/// [`run_action`] only proceeds to `has_open`/the control lease/the throttle
/// — checks for a DIFFERENT, unrelated open ask, or for someone else driving
/// the desktop right now — AFTER this gate returns `Ok`; those are a
/// completely separate concern from "is this call
/// authorized at all".
///
/// Used to return `()` alone, not a distinction between how the `Ok` was
/// reached: this originally returned an
/// `Approval::{Auto,Interactive}` value so every input arm of
/// [`run_action`] could call `activate_target` ONLY for an Interactive
/// approval (a card that actually rendered — a human clicking Weft's own UI
/// to answer it takes the foreground away from the target). That
/// distinction was removed: `activate_target` is now
/// called UNCONDITIONALLY by every input arm regardless of how this call
/// approved — see that function's own doc for why an Auto approval needs the
/// SAME reactivation an Interactive one does. With no caller left that
/// needed to tell the two apart, the `Approval` enum this used to return was
/// deleted entirely rather than kept as unused plumbing.
///
/// now returns `Result<Option<ApprovedWindow>,
/// String>` instead — see [`ApprovedWindow`]'s own doc for the identity-
/// binding problem this closes. `Ok(None)` for every OBSERVE action
/// (`screenshot`/`list_windows`/`cursor_position`) and for any Write action
/// with no window argument at all (`wait`) — nothing to bind. For a Write
/// (input) action WITH a window argument, this now resolves `window_query`
/// AUTHORITATIVELY, right here, the INSTANT authorization actually lands
/// (whether via a standing grant or a human's card) — `Ok(Some(w))` on
/// success, or `Err` (fail-closed, never `Ok(None)`) if THAT resolution
/// itself fails: an input action must never be allowed to proceed at all
/// once its own approval step couldn't even pin down a window identity.
/// [`run_action`] threads the `Some(w)` through to its own later, FRESH
/// `resolve_window` call for that same action and refuses to inject if the
/// two disagree — see [`verify_approved_target`]. That resolution-failure
/// `Err` is NOT returned before authorization — it is deferred behind the
/// same grant/card decision as everything else, so a grant-less caller can't
/// use it as a window-existence oracle; see the `resolution_failure` binding
/// below.
///
/// the resolve above USED TO happen only once
/// authorization already landed (`bind_approved_window`, the earlier
/// helper, now folded directly into this function — see the `resolved`
/// binding below). That left a gap `bind_approved_window` alone could never
/// close: a standing Always grant is looked up by `action_key` ALONE, and
/// the OLD key (`["gui", action, window_query, digest]`) never captured
/// WHICH window was actually approved — just the query STRING. Once a human
/// approved one `type`-into-"notes" call as Always, that SAME key kept
/// auto-approving every FUTURE `type` into whatever window "notes" happens
/// to resolve to later, including an entirely different app/title that
/// closed the original and took its place (the earlier approve→dispatch
/// binding only ever caught that drift AFTER approval, never at the
/// grant-lookup step itself). The fix: for a Write action with a non-blank
/// window argument, resolve `window_query` AUTHORITATIVELY right here,
/// BEFORE `action_key` is even built, and fold the resolved `app`+`title`
/// into the key itself. A standing Always grant then only ever matches the
/// EXACT window identity it was granted for — a query that now resolves to
/// a different app/title mints a DIFFERENT key, misses `auto_decision_exact`
/// entirely, and falls through to a fresh card. This same `resolved` value
/// is reused directly for the `ApprovedWindow` this function returns on
/// EITHER success path (a standing grant, or a human's card) — no second
/// resolve inside a separate `bind_approved_window` step anymore, so there
/// is no "resolve once for the key, resolve again to bind" drift left to
/// reopen.
/// The `Ask.summary` for a `weft_computer` action — data tokens ONLY, no
/// translatable prose (see [`approve`]'s own call site for why). The
/// protocol `action` identifier, plus `@ <window>` when a window is
/// targeted. Shared by BOTH the server-side gate ([`approve`]) and the
/// hook-path summarizer (`bus::server::summarize`) so the two can never
/// drift onto two different label shapes for the same call.
///
/// `#[doc(hidden)] pub` so `bus::server` (a sibling module) builds the
/// SAME label without re-deriving it — the identical single-source-of-truth
/// reasoning as [`computer_session_token`]'s own visibility.
#[doc(hidden)]
pub fn computer_ask_summary(action: &str, window: &str) -> String {
    if window.is_empty() {
        action.to_string()
    } else {
        format!("{action} @ {window}")
    }
}

async fn approve(
    asks: &AskRegistry,
    thread: i32,
    dir: &str,
    wt: Option<i32>,
    action: &str,
    args: &Value,
) -> Result<Option<ApprovedWindow>, String> {
    let window_query = window_arg(args);
    let risk = crate::ask::classify_gui_action(action);
    // NO English prose is constructed here (CLAUDE.md: user-facing strings
    // only via i18n). The visible framing — "<Tool> wants permission …" — is
    // built from the i18n catalogs at each presentation surface (the local
    // Needs-you card prepends the localized tool name + `needs.wantsPermission`;
    // the IM cards render their own localized copy around it). `summary`
    // therefore carries ONLY data tokens the surfaces interpolate verbatim:
    // the protocol `action` identifier (the literal computer-use API verb —
    // `left_click`/`type`/… — a wire token, not translatable prose) and, when
    // present, the target window's own title (user content). No locale can
    // leave this in the wrong language because there is no language in it.
    let summary = computer_ask_summary(action, &window_query);
    let digest = args_digest(args);
    // resolve the window's identity FIRST, before
    // `action_key` is even built — see this function's own doc comment above
    // for the standing-grant identity gap this closes. Every Write-classified
    // action with a non-blank window argument gets this (mirrors the OLD
    // `bind_approved_window`'s own gate exactly): `list_windows`/
    // `cursor_position` and `wait` (Write-classified but windowless) have
    // nothing to bind and keep the OLD, resolve-free key shape below.
    //
    // `screenshot` now ALSO resolves here, even
    // though it's `RiskLevel::ReadOnly` — before this change, ONLY Write
    // actions bound a window at all, so a screenshot's approval card/standing
    // grant was scoped to the bare QUERY STRING alone, never the window it
    // actually resolved to. That let a card opened (or an Always grant
    // earned) for "screenshot @ notes" keep silently authorizing every FUTURE
    // capture of whatever "notes" happens to resolve to later — including a
    // DIFFERENT window that closed the original and took its place while the
    // card sat open, or after the grant was earned. Folding the resolved
    // identity into a screenshot's own key too closes that the same way
    // the input actions' keys already do: a standing grant
    // only ever matches the EXACT window it was granted against, and this
    // same `resolved` value is what the screenshot arm below verifies the
    // window it's ABOUT TO CAPTURE against (see `verify_approved_target`'s
    // new call site there) — fail-closed if a human/Always-approved ONE
    // window and a later capture would land on a DIFFERENT one.
    // `list_windows`/`cursor_position` (no specific window target at all)
    // are unaffected: `window_query` is always blank for them.
    // The resolve below captures its OUTCOME instead of returning a failure
    // early. Returning a resolution failure (WindowNotFound, an ambiguous
    // query) straight to the agent from here — before `auto_decision_gui`
    // was consulted or any card opened — would make this authorization-time
    // resolve a grant-less window-EXISTENCE oracle: probe a query and an
    // instant error vs. an approval wait tells the agent whether such a
    // window is on the human's desktop, reconstructable one probe at a time
    // into exactly the enumeration the `list_windows` card gates (the
    // AmbiguousWindow redaction below strips the candidate LIST on this same
    // path; the exists/doesn't-exist BIT would survive it). A failed resolve
    // therefore rides the SAME approval flow as everything else: `resolved`
    // stays `None`, `action_key` falls back to its query-only shape below, a
    // card opens for the probe itself (no preview — there is no window to
    // show), and the resolution error reaches the agent only AFTER an Allow
    // (auto-grant or human alike; a Deny is byte-identical to any other
    // deny). The in-closure gate errors (stop latch, route revocation) still
    // return immediately — they carry no window-existence information. A
    // query-only Always grant a human mints on such a card only ever
    // fast-paths future FAILING resolves of that same query: any successful
    // resolution builds the id/app/title-bearing key instead, which never
    // matches it.
    let (resolved, resolution_failure) = if !window_query.trim().is_empty()
        && (risk == crate::ask::RiskLevel::Write || action == "screenshot")
    {
        // this
        // authorization-time window resolution is a synchronous `xcap`
        // enumeration that runs for EVERY windowed Write action and every
        // `screenshot`, BEFORE the observe-ask bound or the capture
        // semaphore. Run it on the blocking pool, never inline on an async
        // worker — otherwise concurrent authenticated `tools/call`s (grant-
        // less or standing-granted alike) could each park a tokio worker in
        // enumeration and collectively starve the Stop/Escape kill-switch
        // tasks that MUST keep scheduling. Every other OS-touching call on
        // this server already routes through `on_blocking` for exactly this
        // reason; this approval-time resolve was the one that still ran
        // inline.
        let b = backend::backend();
        let wq = window_query.to_string();
        // this resolve runs
        // at AUTHORIZATION time — before `auto_decision_exact` and before any
        // permission card. `ComputerError::AmbiguousWindow`'s Display lists every
        // matching app name, window title, and id; returning it here would hand
        // that desktop metadata to the agent with NO human approval — exactly the
        // enumeration the `list_windows` card exists to gate, reconstructable via
        // repeated broad-query probes. Redact the candidate list to a generic
        // narrow-your-query message. Other resolution errors (e.g. WindowNotFound)
        // disclose nothing about OTHER windows and pass through unchanged; the
        // full candidate list still reaches the human on the approval card.
        // bound THIS
        // authorization-time enumeration too. It runs BEFORE the screenshot
        // semaphore or the open-approval cap is ever acquired, so a burst of
        // concurrent requests (standing grant or not) could otherwise fan out
        // hundreds of simultaneous xcap enumerations onto tokio's blocking
        // pool. The permit is scoped to this block alone — dropped the moment
        // the resolve returns, NEVER held across the human approval wait
        // below (which can last up to the full ask timeout).
        // re-check the
        // synchronous stop latch AND the direction-precise route revocation
        // INSIDE the closure, after the semaphore/blocking-pool queue waits —
        // exactly like the list_windows/screenshot/cursor closures. The
        // permit bounds concurrency, but bounding IS queueing: a call can now
        // sit parked on the semaphore (or the blocking pool) while a human
        // hits Stop or a delete revokes the route, and without this recheck
        // the eventually-scheduled closure would still enumerate the desktop
        // under an authority that no longer exists.
        let dir_recheck = dir.to_string();
        // Nested `Result`: the OUTER layer is the gate errors
        // (stop/revocation/pool failure — returned immediately via `??`), the
        // INNER layer is the resolution outcome itself, deferred behind the
        // approval decision below.
        let resolution = {
            let _observe_permit = screenshot_semaphore()
                .acquire()
                .await
                .map_err(|e| e.to_string())?;
            on_blocking(move || {
                if computer::stop_latched() {
                    return Err(ComputerError::Disabled.to_string());
                }
                if route_revoked_sync(thread, &dir_recheck, wt) {
                    return Err(SESSION_GONE_MSG.to_string());
                }
                Ok(computer::resolve_window(b.as_ref(), &wq).map_err(|e| match e {
                    // redacted — see the comment above
                    // this block for why the candidate list must not reach the
                    // agent from this authorization-time resolve.
                    ComputerError::AmbiguousWindow { .. } => {
                        "the window query matched more than one window — narrow it to a unique \
                         application name or window title"
                            .to_string()
                    }
                    other => other.to_string(),
                }))
            })
            .await??
        };
        match resolution {
            Ok(w) => (Some(w), None),
            Err(message) => (None, Some(message)),
        }
    } else {
        (None, None)
    };
    // Granularity tradeoff (documented, not a behavior bug): folding
    // `app`+`title` into an input action's key makes a standing Always grant
    // scoped to that EXACT window TITLE, not just the app — a title change
    // (a browser tab navigating, a document renaming itself in its title
    // bar) mints a new key and re-cards even though a human would likely
    // consider it "the same window". That is the safe default this change
    // ships; loosening it to app-only scoping is a legitimate, separately-
    // discussable product tradeoff for later, not something this change
    // changes.
    //
    // the resolved window's own `id` is now ALSO
    // folded in — right after `window_query`, before `app`/`title` — for
    // every action this branch covers (both Write actions with a window, and
    // now `screenshot`). Before this, the key carried `app`+`title` but NOT
    // `id`: if the ORIGINAL window closed and a NEW window opened with the
    // exact same app+title (a relaunched app, a reopened document with an
    // identical name), the key was IDENTICAL to the one a standing Always
    // grant was earned against — silently authorizing input into (or a
    // capture of) the REPLACEMENT window instance, even though
    // `verify_approved_target`'s execution-time check (which DOES compare
    // `id`) would have caught the SAME mismatch had the grant lookup itself
    // been scoped that tightly to begin with. Folding `id` into the key
    // closes that at the SOURCE: a new window instance (new id) mints a
    // DIFFERENT key regardless of how closely its app/title happen to match
    // the old one, missing `auto_decision_exact` and falling through to a
    // fresh card — the safer default for input/capture actions, at the cost
    // of re-prompting once per window instance rather than once per
    // app+title (documented tradeoff, not a bug: see the top-of-file doc for
    // the a `type` Always-grant's own note on this same shape).
    // `always_key_is_persistable` (ask.rs) decides whether a GUI grant is safe to
    // persist from `parts[0]` (`"gui"`) and `parts[2]` (the window query — a
    // window-bound grant is never persisted). Inserting `id` at
    // position 3 (0-indexed) leaves both untouched, so that gate is unaffected by
    // this shape change.
    let action_key = match &resolved {
        Some(w) => crate::ask::action_key(&[
            "gui",
            action,
            &window_query,
            &w.id.to_string(),
            &w.app,
            &w.title,
            &digest,
        ]),
        None => crate::ask::action_key(&["gui", action, &window_query, &digest]),
    };
    // UNREDACTED, even for `action == "type"`: the human approving THIS card
    // needs to see exactly what is about to be typed to judge whether to
    // allow it — a card that hid the text and only said "N characters" would
    // ask for a decision without the information the decision depends on.
    // Only the PERSISTED audit line redacts it, after the fact, once the
    // human has already made that call — see `redact_audit_args`'s doc for
    // the full symmetric point. This is the LOCAL desktop card's own detail —
    // see `detail_redacted`'s doc comment immediately below for the DIFFERENT,
    // EARLIER leak this alone does not close.
    let detail = args.to_string();
    // `AskEvent::Opened` (fired inside
    // `request_with_preview` below) hands this ENTIRE `Ask` — `detail`
    // included — straight to the IM bridge, which renders it into an
    // outbound Lark card BEFORE the human ever answers anything (see
    // `im::outbound::perm_card`). For `action == "type"`, `detail` carries
    // the literal keystrokes about to be typed — a secret typed into a
    // password field would otherwise reach a third party the instant this
    // card opens, regardless of whether the human ever approves it. Reuses
    // the EXACT SAME redaction `redact_audit_args` already applies to the
    // durable audit line (`text` → `{"text_redacted":true,"text_chars":N}`)
    // rather than inventing a second shape — the IM-facing view and the
    // audit-log view have the identical "not the raw text, just its length"
    // requirement, so one function serves both. Every other action's detail
    // carries nothing this module considers secret (a coordinate, a window
    // name, a key combo LABEL like "cmd+s" — never what was typed), so in
    // practice this is `None` for everything except `type` (the condition is
    // structural — "would the audit redaction change the args" —
    // rather than a literal action match; see the comment at the binding
    // below); `im::outbound::perm_card` falls back to the unredacted `detail`
    // when `None` (see its own doc). Passed to the ONLY production caller of
    // `request_with_preview` — see that method's own doc on why it grew this
    // parameter directly rather than a separate `_redacted` variant.
    // populated whenever
    // the audit redaction would change the args AT ALL — not just for `type`.
    // Structurally fail-closed: whatever `redact_audit_args` considers secret
    // (bulk `type` text, a printable `key` chord, a `text` smuggled onto any
    // other action — the last two are also rejected by `pure_validate` before
    // a card ever opens, so this is defense in depth) can never go out raw on
    // the outbound IM card either, and a future redaction rule extends to the
    // IM view automatically instead of needing a second edit here.
    let detail_redacted = {
        let audit_view = redact_audit_args(action, args);
        (&audit_view != args).then(|| audit_view.to_string())
    };

    // GUI actions — observation AND input
    // alike — go through the GUI-only `auto_decision_gui`, NOT the ordinary
    // `auto_decision` every other tool's ask-creation path uses. The
    // difference is deliberate: `auto_decision` also honors the
    // coarse "release all read-only" batch/issue grant for any
    // `RiskLevel::ReadOnly` ask, but `screenshot`/`list_windows` are
    // `ReadOnly` by `classify_gui_action`'s own construction REGARDLESS OF
    // WHICH WINDOW they target — so a session that once released "all
    // read-only" would otherwise silently auto-approve screenshotting or
    // enumerating ANY window on the human's desktop (mail, a browser tab, a
    // password manager, …) with no computer-specific card and no chance for
    // the human to see WHICH window before the pixels are captured. That is
    // a materially larger disclosure than the coarse grant was ever built to
    // cover.
    //
    // `auto_decision_gui` also
    // drops the GLOBAL `dangerous`-mode shortcut that `auto_decision_exact`
    // keeps — Dangerous mode's user-facing promise is worktree-scoped ("act
    // freely inside their worktrees", `i18n` `dangerDesc`), so it must not
    // silently auto-approve desktop-wide GUI control. A GUI action still
    // auto-approves ONLY under a precise Full grant or an EXACT Always-grant
    // `action_key` match (this feature's intended storm-relief valve); a Full
    // grant and exact Always match behave exactly as before, Dangerous mode no
    // longer sweeps GUI in. See `AskRegistry::auto_decision_gui`'s own doc.
    match asks.auto_decision_gui(thread, dir, &action_key) {
        // `resolved` was already computed above,
        // BEFORE this grant was even looked up (the key itself depends on
        // it) — reused directly here rather than resolved a second time, so
        // there is no "resolve once for the key, resolve again to bind" gap
        // left for a window swap to land in.
        // A deferred resolution failure surfaces HERE, once authorization
        // has actually landed — as an `Err`, never `Ok(None)`, which would
        // let a windowed input arm proceed UNBOUND ("approval couldn't pin
        // down a window identity ⇒ the action must not run", this function's
        // fail-closed rule).
        Some(Decision::Allow) => {
            return match resolution_failure {
                Some(message) => Err(message),
                None => Ok(resolved.map(ApprovedWindow::from)),
            }
        }
        // `auto_decision_exact` never actually returns `Deny` today (only
        // Allow-only standing grants exist) — this arm keeps the gate
        // correct regardless, mirroring `handle_ask`'s own defensive shape,
        // rather than silently falling through to a redundant card for it.
        Some(Decision::Deny) => return Err("denied by a standing weft rule".to_string()),
        None => {}
    }

    let preview = preview_for_action(thread, dir, wt, risk, resolved.as_ref());
    // for an input (Write)
    // action, open the card ATOMICALLY with the "no other ask is already open
    // for this (thread, dir)" check — `check_suspended` above and this insert
    // were two SEPARATE lock acquisitions, so two concurrent Write calls for
    // the same session could both pass that pre-check and each open a card. The
    // atomic variant returns `None` (opening nothing) if another card raced in
    // first, which we surface as the SAME `SuspendedPendingAsk` the pre-check
    // and every input arm's own recheck already return. Observe actions
    // (`RiskLevel::ReadOnly` — `screenshot`/`list_windows`/`cursor_position`)
    // are never suspended (see `check_suspended`'s Write-only gate) and keep
    // the plain, always-inserts path.
    let (id, rx) = if risk == crate::ask::RiskLevel::Write {
        match asks.request_with_preview_unless_open(
            thread, dir, "computer", &summary, &detail, detail_redacted.as_deref(), risk, &action_key, preview,
        ) {
            Some(pair) => pair,
            None => return Err(ComputerError::SuspendedPendingAsk.to_string()),
        }
    } else {
        // observe actions
        // (`screenshot`/`list_windows`/`cursor_position`) are deliberately NOT
        // suspended by an unrelated open card the way Write actions are — but
        // that must not mean UNBOUNDED: without a cap, a worker looping
        // grant-less observe calls minted one Ask + waiter per call (each held
        // up to `bus::server::ASK_WAIT`), flooding the registry/UI/IM bridge,
        // and a later Full answer released the whole backlog at once. The
        // bounded variant counts THIS session's open GUI asks and inserts
        // atomically under [`MAX_OPEN_OBSERVE_ASKS`] — over the cap, the call
        // fails closed with a retry hint instead of opening yet another card.
        match asks.request_with_preview_gui_bounded(
            thread, dir, "computer", &summary, &detail, detail_redacted.as_deref(), risk, &action_key, preview,
            MAX_OPEN_OBSERVE_ASKS,
        ) {
            Some(pair) => pair,
            None => {
                return Err(format!(
                    "too many computer-use approvals are already waiting on the human for this \
                     session (max {MAX_OPEN_OBSERVE_ASKS} open at once) — wait for the open cards \
                     to be answered, then retry"
                ))
            }
        }
    };

    // re-check the SYNCHRONOUS
    // stop latch immediately AFTER publishing the card, and self-cancel it if a
    // stop/disable landed. `run_action`'s top `computer::enabled` gate can be
    // passed by a call that then straddles the disable transition — window
    // resolution above is a real OS call, so the gap between "enabled read
    // true" and "card inserted" is unbounded. Both disable paths (Emergency
    // Stop AND, the Settings toggle) run trip_stop_latch →
    // cancel_gui_asks: a card inserted BEFORE the sweep is killed by the sweep;
    // one inserted AFTER it is exactly this straddler — the sweep missed it,
    // but the latch was already visible when we get here (this check is after
    // the insert in program order), so we cancel our OWN card and fail closed.
    // Either interleaving leaves NO surviving card a human could later answer
    // Always/Full into a post-disable grant. `stop_latched` is latch-only and
    // sync (no await between insert and check — nothing can wedge in).
    if computer::stop_latched() {
        asks.cancel(id);
        return Err(ComputerError::Disabled.to_string());
    }
    // the SAME straddler
    // shape, for route revocation — a thread/repo delete whose `asks` purge
    // ran between this call's entry check and the insert above misses THIS
    // card, which would otherwise sit answerable for the full ask timeout
    // after its session's rows are gone (an Always/Full answer minting a
    // grant for a deleted identity). The delete publishes its revocation
    // BEFORE its purge, so program order guarantees one side sees
    // the other: either the purge ran after our insert (and swept the card),
    // or this check — after the insert — sees the already-published
    // revocation and cancels our own card. `route_revoked_sync` is lock-only
    // and sync, same as the latch check above.
    if route_revoked_sync(thread, dir, wt) {
        asks.cancel(id);
        return Err(SESSION_GONE_MSG.to_string());
    }

    match tokio::time::timeout(crate::bus::server::ASK_WAIT, rx).await {
        // same `resolved` value, reused here too —
        // see this function's own top doc comment for why this change
        // deliberately no longer re-resolves a second time at the moment the
        // human's card answers Allow (the earlier `bind_approved_window`
        // used to): a standing Always grant this call might mint is keyed
        // off THIS identity regardless, and `run_action`'s own later, fresh
        // `resolve_window`/`verify_approved_target` pair
        // is what actually
        // guards the approve→dispatch gap a long human wait can open, not
        // this return value.
        // The human answered Allow on the probe's own card — only now may
        // the deferred resolution error reach the agent (see the
        // `resolution_failure` binding's doc above).
        Ok(Ok(Decision::Allow)) => match resolution_failure {
            Some(message) => Err(message),
            None => Ok(resolved.map(ApprovedWindow::from)),
        },
        Ok(Ok(Decision::Deny)) => Err("denied in weft".to_string()),
        // Timed out, or the sender was dropped (`AskRegistry::cancel`/
        // `cancel_for` — e.g. an engine/model switch tearing this session
        // down mid-ask): the SAME explicit, fail-closed deny `bus::server::
        // handle_ask` returns for either case, never an ambiguous default.
        _ => {
            asks.cancel(id);
            Err("no answer in time — denied by default (weft ask bridge)".to_string())
        }
    }
}

/// The window identity [`approve`] itself resolved AUTHORITATIVELY, at the
/// instant it authorized ONE particular input (Write-classified) action.
/// Threaded through to [`run_action`]'s own later,
/// FRESH `resolve_window` call for that SAME action (via
/// [`verify_approved_target`]) so a window that closed, was renamed, or got
/// its id reused by ANOTHER window in the gap between approval and dispatch
/// can never let that action land on the wrong target: the human's card (or
/// standing grant) showed/covered ONE window, so only that exact window may
/// receive the actual click/type/key/etc.
///
/// All three fields are checked (not just `id`): a closed window's id CAN be
/// reused by the OS/window manager for an entirely unrelated window, so `id`
/// alone is not sufficient to prove "this is still the window that was
/// approved" — `app`+`title` must also still match.
///
/// this is now ALSO the exact identity folded
/// into the input action's own `action_key` (see `approve`'s `resolved`
/// binding) — the same three fields serve both jobs (scoping a standing
/// Always grant to one window, and verifying dispatch lands on that same
/// window) since they are, by construction, the identical question: "is
/// this still the window that was approved".
#[derive(Debug, Clone, PartialEq, Eq)]
struct ApprovedWindow {
    id: u32,
    app: String,
    title: String,
}

/// `approve` resolves a Write action's window
/// EXACTLY ONCE (see its own `resolved` binding) and reuses that SAME
/// `WindowInfo` both to build `action_key` and — via this trivial,
/// infallible conversion — to construct the `ApprovedWindow` it returns.
/// Replaces the earlier `bind_approved_window`, which used to re-resolve
/// `window_query` a SECOND time at the moment authorization landed; folding
/// window identity into the key itself (this change's fix) means that second
/// resolve no longer serves any purpose a plain field-copy can't.
impl From<computer::WindowInfo> for ApprovedWindow {
    fn from(w: computer::WindowInfo) -> Self {
        ApprovedWindow { id: w.id, app: w.app, title: w.title }
    }
}

/// the execution-time check every input arm of
/// [`run_action`] runs right after its OWN fresh `resolve_window` (and right
/// BEFORE target activation/the actual backend injection) — the target
/// this call is about to drive must be BYTE-FOR-BYTE the same window
/// [`approve`] itself bound at authorization time (`id`+`app`+`title` all
/// three — see [`ApprovedWindow`]'s own doc for why `id` alone isn't
/// sufficient). `approved: &None` (an observe action, or a Write action with
/// no window arg at all) always passes — nothing was bound, so there's
/// nothing to verify. A mismatch fails CLOSED with a message aimed at the
/// calling AGENT (MCP result text, not an i18n-routed UI string) telling it
/// to re-run so the human re-approves whatever the CURRENT target actually
/// is, rather than silently proceeding against a replaced window.
fn verify_approved_target(
    approved: &Option<ApprovedWindow>,
    w: &computer::WindowInfo,
) -> Result<(), String> {
    let Some(ap) = approved else {
        return Ok(());
    };
    if w.id != ap.id || w.app != ap.app || w.title != ap.title {
        return Err(
            "the target window changed since this action was approved — re-run so the human \
             re-approves the current target"
                .into(),
        );
    }
    Ok(())
}

/// resolve `window_query` fresh against the
/// process-wide backend, record its id into `window_id_out` — done even when
/// the very next [`verify_approved_target`] check fails, so the audit log
/// still names which window was TARGETED, not only ones that actually
/// received input — then verify it against `approved`. Every input arm of
/// [`run_action`] resolves TWICE: once BEFORE the paced dispatch closure
/// (via [`resolve_and_verify_target_blocking`] — purely to get a target to
/// activate, plus an early fail-fast identity check and the arm's advisory
/// preflight), and again INSIDE
/// [`pace_activate_verify_and_inject`]'s closure, immediately after
/// activation and immediately before the backend call.
///
/// Why twice: the pacing sleep and `activate_target` (a potentially slow,
/// blocking OS shell-out — `osascript`/`wmctrl`/`xdotool --sync`) both take
/// real wall-clock time — the window can move, resize, close, or have its
/// id reused by an entirely unrelated window WHILE they run. A coordinate
/// mapped, or a focus-freshness check made, against the PRE-pacing
/// resolve could then land outside the window that's actually there once
/// activation finishes, or silently target a replacement window that reused
/// the same id — with no re-check that it's still the one `approve` bound at
/// authorization time. The SECOND (in-closure) call's `WindowInfo` is what
/// every arm actually maps coordinates against / checks focus-freshness
/// against / injects into; the first call exists only to obtain a target
/// for activation and an early fail-fast identity check.
fn resolve_and_verify_target(
    window_query: &str,
    approved: &Option<ApprovedWindow>,
    window_id_out: &mut Option<u32>,
) -> Result<computer::WindowInfo, String> {
    let w = computer::resolve_window(backend::backend().as_ref(), window_query).map_err(|e| e.to_string())?;
    *window_id_out = Some(w.id);
    verify_approved_target(approved, &w)?;
    Ok(w)
}

/// the on-blocking-pool wrapper every
/// input arm of [`run_action`] calls (instead of [`resolve_and_verify_
/// target`] directly) for its PRE-pacing resolve — window
/// enumeration (`computer::resolve_window`, inside the wrapped call) is a
/// synchronous OS call that must not run straight on the async worker (see
/// [`on_blocking`]'s own doc for why); the AUTHORITATIVE second resolve
/// happens inside [`pace_activate_verify_and_inject`]'s own blocking
/// closure, which calls the sync fn directly since it is already on the
/// blocking pool. [`resolve_and_verify_target`] itself
/// takes `window_id_out` as `&mut Option<u32>` — a reference into the
/// CALLER's own stack frame, which can't cross into a `'static` blocking
/// closure — so this runs it against a throwaway LOCAL `Option<u32>` inside
/// the closure instead, and writes that back into the real `window_id_out`
/// itself once `on_blocking` returns, preserving `resolve_and_verify_
/// target`'s existing "record the id even when identity verification fails"
/// semantics exactly. A drop-in replacement for every existing call site —
/// same arguments, same `Result`, just `async` now.
async fn resolve_and_verify_target_blocking(
    window_query: &str,
    approved: &Option<ApprovedWindow>,
    window_id_out: &mut Option<u32>,
) -> Result<computer::WindowInfo, String> {
    let window_query = window_query.to_string();
    let approved = approved.clone();
    let (id, result) = on_blocking(move || {
        let mut id = None;
        let result = resolve_and_verify_target(&window_query, &approved, &mut id);
        (id, result)
    })
    .await?;
    *window_id_out = id;
    result
}

/// Map an agent-given screenshot-space coordinate for `w` — the FRESHLY
/// resolved (post-activation) window every coordinate-taking input arm
/// already has on hand — to a physical on-screen point, using `(thread,
/// dir, w.id)`'s most recently recorded screenshot dimensions
/// rather than re-deriving a scale from `w`'s CURRENT size —
/// see `computer::map_screenshot_coord`'s own doc for the resize-drift bug
/// this replaces `computer::map_to_physical` to close.
///
/// Fails CLOSED — a clear, agent-facing error, never a silent fallback to
/// "current size" — when NOTHING is on file for this exact `(thread, dir,
/// w.id)`: an agent that never screenshotted `w.id` this session (or whose
/// last screenshot fell out of `computer::MAX_SHOT_DIMS`'s bound) has no
/// screenshot-space coordinate system to interpret its own `coordinate`
/// against at all — this ALSO happens to enforce "screenshot before you
/// click", a good practice this change is happy to require outright rather
/// than merely encourage.
fn map_input_coord(thread: i32, dir: &str, wt: Option<i32>, w: &computer::WindowInfo, cx: u32, cy: u32) -> Result<(i32, i32), String> {
    // `shot_dims_for` (not the old id-only
    // `shot_dims`) also verifies `w`'s CURRENT app+title against whatever was
    // recorded at capture time — an id the OS reused for a different window
    // since that screenshot now reads as no record at all, the same
    // fail-closed message below, rather than a stale hit against the OLD
    // window's saved geometry. See that function's own doc.
    let (shot_w, shot_h) = computer::shot_dims_for(thread, dir, wt, w).ok_or_else(|| {
        format!(
            "no recent screenshot of window {} to map this coordinate against — take a screenshot of \
             it first, then read coordinates off THAT screenshot",
            w.id
        )
    })?;
    computer::map_screenshot_coord(w, shot_w, shot_h, cx, cy).map_err(|e| e.to_string())
}

/// Fixed key order for the "consequential parameters" digest folded into the
/// Always-grant `action_key` — every argument
/// across every action this server implements that actually determines WHAT
/// an action does to the human's screen. `text` carries `type`/`key`'s own
/// content; the rest are the coordinate/scroll/duration shapes the other
/// actions use. Anything NOT in this list (today: `action`, `window` — both
/// already folded into `action_key` as their own, separate parts — and
/// nothing else this server's schema defines) never affects the digest.
const DIGEST_PARAM_KEYS: &[&str] = &[
    "coordinate",
    "start_coordinate",
    "text",
    "scroll_direction",
    "scroll_amount",
    "duration_ms",
];

/// A stable sha256 hex digest of `args`'s consequential parameters,
/// folded as the trailing element of the Always-grant
/// `action_key` so a standing grant is scoped to the EXACT parameters it was
/// granted for — not just `action` + `window`. Before this, `action_key =
/// ["gui", action, window]` meant a SINGLE Always answer to one `type` call
/// covered EVERY future `type` into that window with ANY text: an
/// Always-grant earned by approving "type your name into the search box"
/// would silently also auto-approve "type your password into the search
/// box" the next time an agent decided to send that, since the key never
/// captured what was actually typed. Same hazard, smaller blast radius, for
/// every other action's own consequential params (a click's coordinate, a
/// scroll's direction/amount, a drag's endpoints).
///
/// Builds a JSON object over [`DIGEST_PARAM_KEYS`] ONLY (never the raw
/// `args`, which also carries `action`/`window` — already their own, separate
/// `action_key` parts, so folding them in again here would be redundant, not
/// wrong, but also not needed), omitting any key `args` doesn't carry (a
/// `left_click`'s absent `text` must not hash the same as an explicit
/// `text: null`, and must never appear at all for an action that has no
/// `text`), then sha256-hexdigests that JSON's serialized bytes.
/// `serde_json::Map` has no `preserve_order` feature enabled in this crate
/// (see `Cargo.toml`), so its `Value::Object` serialization is ALREADY
/// stable key-sorted regardless of insertion order — this still inserts via
/// the fixed `DIGEST_PARAM_KEYS` order for readability, not because
/// correctness depends on it.
///
/// The DIGEST — never the raw JSON — is what actually lands in the
/// persisted `action_key`: unlike `detail` (the full unredacted params
/// `approve` shows the human on the card itself, which is ephemeral and gone
/// the instant the card resolves — see the inline comment at its own
/// definition above), an Always-grant's `action_key` is retained in memory
/// for the life of the session, so a literal `type` payload sitting in that
/// key would persist exactly the plaintext `redact_audit_args` exists to
/// keep OUT of the durable audit log, just relocated to a different
/// in-memory table. The digest gets the identical narrowing power (the same
/// call reuses the grant; ANY changed consequential param mints a new key,
/// forcing a fresh card) with none of that exposure — a card built from the
/// digest alone cannot be reversed back into the text that produced it.
///
/// `#[doc(hidden)] pub` (not `pub(crate)`): `tests/computer_mcp.rs` is a
/// separate integration-test crate that needs to pre-seed an `AlwaysGrant`
/// whose `action_key` matches EXACTLY what [`approve`] would itself compute
/// for a given call — mirrors `computer::mock::MockBackend`'s own doc
/// comment on why a test-only helper is `#[doc(hidden)] pub` rather than
/// `#[cfg(test)]`.
#[doc(hidden)]
pub fn args_digest(args: &Value) -> String {
    let mut canonical = serde_json::Map::new();
    for key in DIGEST_PARAM_KEYS {
        if let Some(v) = args.get(*key) {
            canonical.insert((*key).to_string(), v.clone());
        }
    }
    let canonical_json = Value::Object(canonical).to_string();
    let digest = Sha256::digest(canonical_json.as_bytes());
    format!("{digest:x}")
}

/// The screenshot preview to attach to a NEW Ask card for `action` targeting
/// `window_query`, if any. Observe-only actions
/// (`risk != Write`) never attach one, unchanged from the M3-B rule this
/// replaces (relocated here from `bus::server::handle_ask` — see this
/// module's own doc comment): the agent that just took a screenshot already
/// has it, attaching it again is pure payload with no new context for the
/// human. For a Write-classified action, the preview registry now carries
/// the window id the LAST screenshot for this `(thread, dir)` actually
/// resolved to (see [`store_screenshot_preview`] / [`last_screenshot_
/// preview`]) — this only attaches it when THIS action's own target window
/// resolves to that SAME id, so an input action on a DIFFERENT window than
/// the one last screenshotted never shows a stale, unrelated preview.
/// Resolution failure (an empty/missing query, no match, an ambiguous
/// match) is silently treated as "no preview to attach" — the real error for
/// a malformed/unresolvable window surfaces later, at dispatch time
/// ([`required_window`] / `resolve_window`), once the human has already
/// answered this card.
fn preview_for_action(
    thread: i32,
    dir: &str,
    wt: Option<i32>,
    risk: crate::ask::RiskLevel,
    resolved: Option<&computer::WindowInfo>,
) -> Option<String> {
    if risk != crate::ask::RiskLevel::Write {
        return None;
    }
    // REUSE the identity
    // `approve` already resolved once (on the blocking pool — see its
    // `resolved` binding) instead of enumerating windows a SECOND time inline
    // on the async runtime. Under several concurrent grant-less input requests
    // that extra synchronous `xcap` enumeration could occupy every tokio worker
    // before the first card was even admitted, starving the Stop/Escape tasks;
    // reusing the already-resolved window removes it (and closes the last
    // "resolve twice, window swapped in between" gap for the preview). Match
    // on the FULL window identity (id + app + title),
    // not the numeric id alone — see [`VerifiedWindowIdentity`]'s own doc for
    // the id-reuse hazard an id-only comparison left open.
    let target = VerifiedWindowIdentity::from_window(resolved?);
    let (data_uri, stored) = last_screenshot_preview(thread, dir, wt)?;
    (stored == target).then_some(data_uri)
}

/// A window-scoped action's `window` argument, validated BEFORE it ever
/// reaches [`computer::resolve_window`]: missing,
/// non-string, empty, or all-whitespace all fail here with an explicit
/// missing-argument error instead of falling through to resolution. Without
/// this check, an empty string would still reach `resolve_window`'s
/// substring match, where an empty needle's `str::contains("")` is trivially
/// true for every window — silently resolving to whichever ONE window
/// happens to be visible (rather than reporting the missing argument, or
/// erroring honestly as `ComputerError::AmbiguousWindow` would whenever more
/// than one window is visible). Applies to every window-scoped action —
/// `screenshot` and every input action except `cursor_position`/`wait`,
/// which take no `window` at all and never call this.
fn required_window(args: &Value) -> Result<&str, String> {
    match args.get("window").and_then(|v| v.as_str()) {
        Some(w) if !w.trim().is_empty() => Ok(w),
        _ => Err(
            "missing required 'window': a window id from list_windows, or a case-insensitive \
             substring of its app name or title"
                .to_string(),
        ),
    }
}

/// The audit log's OWN args payload for this call — identical to the raw
/// call args for every action except `type`, whose literal keystrokes must
/// NEVER be written to `.weft/computer-audit.jsonl`:
/// that file persists on disk indefinitely and is readable by anyone with
/// filesystem access to the worktree, unlike the Ask card's `detail` (built
/// in [`approve`], from this SAME raw `args`) — which is ephemeral, shown
/// only to the human deciding whether to allow it, and gone the moment the
/// card resolves. The SAME raw text serves two different purposes with two
/// different retention needs, so it gets two different treatments: the card
/// needs it to let the human judge WHAT is about to be typed; the durable
/// log does not need to carry it forward forever just to record THAT a type
/// action happened, how many characters, and whether it succeeded. `text`
/// becomes `{"text_redacted": true, "text_chars": N}` in place of the
/// literal string; every other key (`action`, `window`, …) is untouched.
fn redact_audit_args(action: &str, args: &Value) -> Value {
    let mut redacted = args.clone();
    let Some(obj) = redacted.as_object_mut() else {
        return redacted;
    };
    // strip every key
    // outside the action's [`allowed_args`] allowlist WHOLESALE — both the
    // key name and its value are request-author-chosen and can carry smuggled
    // content, so neither may persist. `reject_unknown_args` already refuses
    // such a request before any card, but the REJECTED attempt is audited
    // too; only the count survives, as `unrecognized_args_redacted`.
    let allowed = allowed_args(action);
    let unknown: Vec<String> = obj
        .keys()
        .filter(|key| !allowed.iter().any(|a| *a == key.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        for key in &unknown {
            obj.remove(key);
        }
        obj.insert("unrecognized_args_redacted".to_string(), json!(unknown.len()));
    }
    // Only redact when `text` is ACTUALLY present as a string — a malformed
    // call missing it entirely (rejected by `required_text` before it ever
    // reaches the backend, see the "type"/"key" arms of `run_action`) must not
    // have a synthetic `text` key manufactured into its audit record that was
    // never in the real request.
    let Some(text) = obj.get("text").and_then(|v| v.as_str()) else {
        return redacted;
    };
    // `type` always redacts (bulk keystrokes are content). `key` redacts a
    // printable TEXT-entry
    // chord — judged by [`is_printable_text_chord`] (a chord like
    // computer_srv.rs:3162), the same semantic predicate `reject_unsafe_key_
    // combo` rejects on, so even the rejected attempt's audit line never
    // records the raw character(s); an UNPARSEABLE `key` payload is redacted
    // too (it is not a command chord the audit needs for forensics, and could
    // be anything — a secret pasted into the wrong field included). A real
    // command combo (`cmd+s`, `ctrl+c`, `enter`) is NOT content and stays in
    // the audit for forensics.
    //
    // EVERY OTHER action
    // redacts a present `text` unconditionally — no other action consumes the
    // field at all (`pure_validate` now rejects it outright), but the audit
    // logs rejected calls too, so a payload smuggled onto e.g. a `screenshot`
    // would otherwise land verbatim in the durable log the `type` redaction
    // exists to keep it out of.
    let redact = match action {
        "type" => true,
        "key" => match computer::parse_key_combo(text).as_deref() {
            Ok(tokens) => is_printable_text_chord(tokens),
            Err(_) => true,
        },
        _ => true,
    };
    if redact {
        let chars = text.chars().count();
        obj.insert("text".to_string(), json!({ "text_redacted": true, "text_chars": chars }));
    }
    redacted
}

// —— type/key focus-freshness gate ——
//
// Neither backend this module drives exposes a real "which window currently
// holds OS keyboard focus" query (`xcap` has none; a genuine focus API is a
// real-machine follow-up), so `type`/`key` cannot verify focus
// directly. Without SOME check here, they would blindly inject keystrokes
// into whatever happens to be focused — which could be a credential prompt,
// or any other window the agent never intended, if it never actually clicked
// its target first. This substitutes a WEFT-tracked freshness heuristic
// instead of real focus verification: a `left_click`/`right_click`/
// `double_click`/`triple_click` that actually reached the OS is PRESUMED to
// have handed that window OS focus (an OS-level guarantee this module does
// not itself re-verify — a third party can still steal focus back after the
// click, and this module would have no way to know), and `type`/`key`
// require ONE such click, on the SAME resolved window, for the SAME
// `(thread, dir)`, within the last `FOCUS_FRESHNESS_MS`. This is not a
// substitute for real focus verification — it is a floor, not a ceiling —
// but it closes the main accident surface: typing into a target window that
// was never clicked at all, this session, ever.
//
// A previous design patched a SECOND hole here — an
// Interactive approval card dragging OS focus to Weft between the click and
// the keystrokes — by replaying the exact last-click coordinate right before
// `type`/`key`. The activation design REMOVES that replay hack: it only ever helped
// `type`/`key` (a `left_click`/`scroll`/`drag`/`mouse_move` itself was still
// exposed — an absolute-coordinate action can land on Weft's own card if the
// card now covers the target window's on-screen position, not just a stale
// focus target), and replaying a synthetic click is itself not side-effect-
// free (it can collapse a double-click text selection, or re-toggle a
// checkbox/button the agent never asked to click again). See
// `activate_target`'s own doc, right below this section, for the actual fix:
// reactivating the TARGET window through `backend::ComputerBackend::
// activate_window` before ANY input action reaches the OS, not just
// `type`/`key` — and, review P1 §6, before an AUTO-approved
// input action too, not only one that actually surfaced an Interactive card.

/// How long a click on a window is trusted to still hold that window's OS
/// focus for a subsequent `type`/`key` — see this section's own doc comment.
const FOCUS_FRESHNESS_MS: u64 = 15_000;
const FOCUS_FRESHNESS_SECS: u64 = FOCUS_FRESHNESS_MS / 1000;

/// Process-level "last window this `(thread, dir)` actually clicked, and
/// when" registry — see this section's own doc comment.
/// no longer carries the click's physical
/// `(px, py)` — that existed ONLY to feed the replay-click
/// hack this section's own top doc comment describes removing; the
/// window id + timestamp pair is all [`require_recent_focus`] itself has
/// ever needed.
///
/// the timestamp is a
/// monotonic [`std::time::Instant`] now, NOT wall-clock `now_ms()`. This used
/// to reason "a system clock adjustment mid-session is not a hazard this
/// heuristic needs to defend against" — wrong for a PERMISSION-adjacent gate:
/// with wall clock, a backward clock step (manual correction, NTP sync) made
/// `now_ms().saturating_sub(ts)` read 0, so ONE old click stayed "fresh" for
/// as long as the rollback was large — letting `type`/`key` bypass the
/// documented 15-second click requirement for minutes or hours. `Instant` is
/// process-local and monotonic, which is exactly what this check is: a
/// process-local freshness heuristic. (The audit log's `ts_ms` stays wall
/// clock — that's a human-readable record, not a gate.)
/// the value carries the
/// clicked window's FULL [`VerifiedWindowIdentity`] now, not its bare id — an
/// id reused within the 15s freshness window used to let the REPLACEMENT
/// window read as "recently clicked", and under a Full grant a `type`/`key`
/// would then activate and inject into it with neither a card nor a genuine
/// click. Identity comparison fails that closed, same as the preview and
/// shot-dims registries already do.
/// Keyed by `(thread, dir, wt)` — the worktree id is part of the key because
/// sibling workers of ONE
/// multi-repo direction share `(thread, dir)` but are distinct sessions, so
/// worker A's successful click must never satisfy worker B's `type`/`key`
/// focus prerequisite — under a shared Full grant that meant silently
/// injecting keystrokes on another session's clicked window. The same
/// widening the bearer, lease, shot-dims, and preview registries already got.
fn recent_clicks(
) -> &'static Mutex<HashMap<(i32, String, Option<i32>), (VerifiedWindowIdentity, std::time::Instant)>> {
    static CLICKS: OnceLock<
        Mutex<HashMap<(i32, String, Option<i32>), (VerifiedWindowIdentity, std::time::Instant)>>,
    > = OnceLock::new();
    CLICKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a SUCCESSFUL click on `window_id` for `(thread, dir, wt)` — called
/// ONLY from the click-family arm of [`run_action`], and ONLY after the
/// backend call itself returned `Ok`: a rejected/failed click never actually
/// touched the real window, so it must not seed a false freshness record for
/// a later `type`/`key`.
fn record_click_focus(thread: i32, dir: &str, wt: Option<i32>, w: &computer::WindowInfo) {
    let mut g = recent_clicks().lock().unwrap_or_else(|e| e.into_inner());
    g.insert(
        (thread, dir.to_string(), wt),
        (VerifiedWindowIdentity::from_window(w), std::time::Instant::now()),
    );
}

/// `type`/`key`'s pre-execution gate: reject unless a click on THIS EXACT
/// resolved `window_id`, for THIS `(thread, dir, wt)`, landed within the last
/// [`FOCUS_FRESHNESS_MS`] — see this section's own doc comment for what this
/// is (and is not) verifying.
fn require_recent_focus(
    thread: i32,
    dir: &str,
    wt: Option<i32>,
    w: &computer::WindowInfo,
) -> Result<(), String> {
    let g = recent_clicks().lock().unwrap_or_else(|e| e.into_inner());
    let target = VerifiedWindowIdentity::from_window(w);
    // `Instant::elapsed` is monotonic — a wall-clock
    // rollback can no longer stretch one click's freshness window. The FULL
    // identity must match, not just the id (see
    // [`recent_clicks`]'s own doc for the id-reuse hazard).
    let fresh = matches!(
        g.get(&(thread, dir.to_string(), wt)),
        Some((clicked, ts)) if *clicked == target
            && ts.elapsed() <= std::time::Duration::from_millis(FOCUS_FRESHNESS_MS)
    );
    if fresh {
        return Ok(());
    }
    let window_id = w.id;
    Err(format!(
        "window {window_id} doesn't appear to have OS focus yet — click inside the target window \
         first to focus it, then type/key within {FOCUS_FRESHNESS_SECS}s"
    ))
}

// —— reclaiming the foreground before every input action ——

/// Reactivates the target window before the backend ever touches the OS —
/// click family, `mouse_move`, `left_click_drag`, `scroll`, `type`, `key`
/// (replacing the click-replay
/// hack — see the focus-freshness section's own doc comment above for why
/// that hack was unsafe and insufficient). Called via
/// [`pace_activate_verify_and_inject`] — NOT this function directly — from
/// every input arm of [`run_action`]: inside the paced dispatch closure,
/// AFTER the pacing sleep and its stop/lease recheck, and immediately
/// followed by the post-activation recheck + the authoritative fresh
/// resolve before the action-specific backend call itself — see that
/// function's own doc for why THIS call, being a potentially slow, blocking
/// OS call (`osascript`/`wmctrl`/`xdotool`), needs its own dedicated
/// recheck after it rather than trusting the one already taken before it
/// started.
///
/// An earlier shape ran this ONLY for an Interactive approval (a card that
/// actually rendered, so a human clicking Weft's own UI to answer it just
/// took the foreground away from the target). It covers EVERY input
/// action, Auto-approved ones included:
/// a standing grant deciding silently does NOT
/// mean the target window still holds the real OS foreground/focus at the
/// moment this call finally runs — a human sitting at the machine can switch
/// windows, alt-tab, or bring some OTHER app forward between an agent's calls
/// for reasons that have nothing to do with weft at all, and an Auto approval
/// (unlike an Interactive one) has no way to know whether that happened. So
/// this is now called UNCONDITIONALLY for every input action rather than
/// branching on how the call was approved — the (former) `Approval`
/// Auto/Interactive split existed ONLY to gate this call, and reactivating an
/// already-frontmost window is a cheap, idempotent no-op on every backend
/// this module drives, so there is no real cost to doing it every time
/// instead of only when a card is known to have stolen focus.
///
/// FAILS CLOSED, never falls through to the real action: `Unsupported` (no
/// `computer-os` feature, or a real backend that couldn't find a window-
/// activation API at all — see `backend::ComputerBackend::activate_window`'s
/// own doc) or any other backend error both propagate as this function's own
/// `Err`, naming why (the target window couldn't be reactivated) and the way
/// around it: answer/grant from weft's own desktop UI, where the foreground
/// never has anywhere else to go.
///
/// KNOWN, ACCEPTED residual (recorded here, not
/// eliminated this change): even after this call succeeds, there is no
/// cross-platform primitive this module can call to VERIFY the target window
/// is truly frontmost at the exact instant the backend call right after this
/// one actually injects — neither `xcap` nor `enigo` exposes a "is this
/// window frontmost right now" query, so a real THIRD-PARTY focus-stealing
/// race (some other app/window grabbing focus back) in the gap between this
/// call returning `Ok` and the very next backend call could still, in
/// principle, land the input elsewhere. This closes the ORDINARY case (an
/// agent that never re-activates at all, or one that only did so for a
/// card-driven Interactive approval) — it is a floor, not a ceiling, exactly
/// like [`require_recent_focus`]'s own doc says about the freshness
/// heuristic it complements. This residual is scoped to third-party focus
/// theft specifically — a human hitting Stop DURING this call is a
/// DIFFERENT, now-closed hazard: see [`pace_activate_verify_and_inject`]'s
/// own doc for the post-activation recheck that closes it.
fn activate_target(target: &computer::WindowInfo) -> Result<(), String> {
    // the FULL verified
    // identity crosses this boundary now, not the bare id — the backend
    // re-verifies app/title as close to the raise as the platform allows
    // (see `ComputerBackend::activate_window`'s doc), so an id reused while
    // the activation closure sat queued can no longer have the REPLACEMENT
    // application raised/focused under the original approval.
    let target_id = target.id;
    backend::backend().activate_window(target).map_err(|e| {
        format!(
            "window {target_id} couldn't be activated before this input action ({e}) — answer \
             from weft's own desktop UI instead, where the foreground never has anywhere else to go"
        )
    })
}

// —— screenshot → MCP image content + Ask-card preview registry ——

/// Long edge / JPEG quality for the MCP `image` content block a `screenshot`
/// result gets, for the engines [`engine_accepts_mcp_image`] allows — a full
/// working-context image for the MODEL to reason over, so it stays at the
/// SAME long edge `screenshot_window`'s own on-disk downscale already caps
/// at (`computer::display_scale`'s `MAX_LONG_EDGE`) — this only ever shrinks
/// further on a window that somehow still exceeds it after that.
const MCP_IMAGE_LONG_EDGE: u32 = 1280;
const MCP_IMAGE_QUALITY: u8 = 75;

/// Long edge / JPEG quality for the Ask-card preview registry's thumbnail —
/// deliberately smaller and lower-quality than the MCP image block above:
/// this one is glance-level visual context for a HUMAN triaging a permission
/// card, not something a model reasons over pixel-by-pixel, so a smaller
/// payload is the right tradeoff.
const PREVIEW_LONG_EDGE: u32 = 640;
const PREVIEW_QUALITY: u8 = 60;

/// process-wide cap on CONCURRENT screenshot
/// capture+encode. A `screenshot` call synchronously captures a full RGBA
/// frame, PNG-encodes it to disk, then may ALSO JPEG-encode it up to twice
/// more (the preview thumbnail above, and — engine-gated — the MCP inline
/// image) — with no throttle/semaphore/flight-guard anywhere on this path
/// before this change. Full access or a matching Always grant lets a worker
/// fire arbitrarily many concurrent `screenshot` calls, each one holding its
/// own full-resolution RGBA buffer (tens of MB for a 4K display) plus
/// PNG/JPEG encode buffers, all resident in memory at once — a
/// straightforward memory/thread-exhaustion vector.
///
/// `N = 2`: generous enough that one legitimate in-flight capture never
/// queues behind an unrelated session's for long (this is an occasional,
/// human-paced action, not a hot loop), small enough that the worst case is
/// bounded to a couple of full-frame buffers rather than however many
/// concurrent MCP calls a worker cares to fire. A queued caller beyond the
/// cap waits its turn rather than allocating its own buffers alongside the
/// others.
const SCREENSHOT_CONCURRENCY: usize = 2;

/// The semaphore [`SCREENSHOT_CONCURRENCY`] backs — acquired by the
/// `screenshot` arm of `run_action` immediately before the synchronous
/// capture, held across every encode that same call does, released when
/// that arm's block ends (including on an early `?` return from a failed
/// capture — Rust drops the permit as part of normal scope unwinding, no
/// manual guard-drop needed).
///
/// the `list_windows` arm of `run_action`
/// now ALSO acquires this same semaphore, before its own enumeration —
/// deliberately sharing this one "OS observation" budget rather than minting
/// a second, separate one. `MAX_OPEN_OBSERVE_ASKS` only bounds
/// how many observe Ask CARDS may sit open waiting on a human; a session
/// already holding a standing Full/Always grant skips card-opening entirely,
/// and until this change had NO concurrency cap at all on the OS calls that
/// path could fire. This is the hard ceiling for that already-authorized
/// case — `MAX_OPEN_OBSERVE_ASKS` and this semaphore guard two different
/// stages of the same lifecycle (before a grant exists, and after one does).
fn screenshot_semaphore() -> &'static tokio::sync::Semaphore {
    static SEM: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    SEM.get_or_init(|| tokio::sync::Semaphore::new(SCREENSHOT_CONCURRENCY))
}

/// Strip the `data:image/jpeg;base64,` prefix `encode_jpeg_data_uri` always
/// adds — the MCP `image` content type wants the RAW base64 payload with no
/// prefix (`{"type":"image","data":"<base64>","mimeType":"image/jpeg"}`),
/// unlike a `data:` URI meant to sit directly in something like `<img src>`.
fn strip_data_uri_prefix(uri: &str) -> Option<&str> {
    uri.strip_prefix("data:image/jpeg;base64,")
}

/// The `tool` currently driving `(thread, dir)` — the SAME durable field
/// `commands::session_meta` reads (`direction.tool`; see that entity field's
/// own doc comment on why it — not `session.tool` — is the per-task engine
/// choice of record), so a worker mid-engine-switch reports the tool that
/// will actually see this screenshot. `dir == bus::LEAD` has no direction row
/// at all (a lead joins the bus under the `LEAD` sentinel, not a numeric
/// direction id — mirrors [`session_root`]'s own dir=="lead"-vs-numeric
/// split), so it reads the thread's `lead_tool` instead. `None` on any lookup
/// failure — a `dir` that fails to parse, a numeric `dir` that doesn't
/// resolve to a direction belonging to THIS thread (a stale/forged route,
/// same check [`session_root`] makes), a missing row, or a DB error — never a
/// guess.
async fn session_tool(db: &Db, thread: i32, dir: &str) -> Option<String> {
    if dir == crate::bus::LEAD {
        return repo::get_thread(db, thread).await.ok().flatten().map(|t| t.lead_tool);
    }
    let direction_id = dir.parse::<i32>().ok()?;
    match repo::get_direction(db, direction_id).await {
        Ok(Some(d)) if d.thread_id == thread => Some(d.tool),
        _ => None,
    }
}

/// Which weft_computer callers get a screenshot's pixels as an inline MCP
/// `image` content block, on top of the text confirmation
/// EVERY caller always gets regardless (this module's own doc comment: the
/// agent's own image-viewing tool is the universal fallback):
///
/// | `session_tool` result           | MCP image block? |
/// |----------------------------------|-------------------|
/// | `"claude"`                       | yes — claude feeds an MCP `image` content block straight to the model |
/// | any ACP backend (`acp::backend_for(tool).is_some()`, today just `"omp"`) | yes — same reasoning as claude |
/// | `"codex"` / `"opencode"`         | no — MCP image-content support isn't confirmed from this repo's own code for either, so this stays fail-safe |
/// | lookup failure (`None`)          | no — same fail-safe default |
///
/// The text path is NEVER dropped for the "no" rows — see
/// [`text_and_image_result`]'s doc.
async fn engine_accepts_mcp_image(db: &Db, thread: i32, dir: &str) -> bool {
    match session_tool(db, thread, dir).await {
        Some(tool) => tool == "claude" || crate::acp::backend_for(&tool).is_some(),
        None => false,
    }
}

/// Process-level "most recent screenshot" registry
/// : one small preview thumbnail PLUS the
/// window id it actually came from, per (thread, dir), refreshed on EVERY
/// successful `screenshot` action regardless of which engine is asking
/// (unlike the MCP image content block above, which is engine-gated) —
/// [`preview_for_action`] attaches it to a `weft_computer` GUI INPUT Ask
/// card ONLY when that action's own target window resolves to the SAME id
/// (never an observe-only ask, and never a DIFFERENT window's stale preview)
/// so the human has visual context before allowing/denying a click/type/
/// key/etc without opening the saved PNG themselves. A process-wide
/// `OnceLock`, not per-request state, so it survives across the many
/// separate MCP calls a session makes — mirrors `computer::control_mutex`'s
/// own process-level-static shape for the same reason.
/// In-memory only: a stale/missing preview is harmless (the Ask card just
/// renders without one), so a restart starting empty is fine — no
/// durability needed.
///
/// this used to grow UNBOUNDED for the life of the
/// process — one entry per (thread, dir) that ever screenshotted, never
/// evicted. [`MAX_PREVIEWS`] caps it; the value's third element is the
/// insertion timestamp [`store_screenshot_preview`] needs to find (and evict)
/// the single oldest entry once the map is full — see [`evict_oldest_if_full`].
/// The FULL identity of a window one of this module's per-session registries
/// recorded — `id` ALONE is never enough: an OS window id is reusable, so a
/// closed window's number can be handed to a DIFFERENT window (different
/// app/title) moments later, and an id-only match would silently transfer
/// whatever the registry vouches for onto that replacement. Comparing
/// `app`+`title` alongside `id` fails such a match closed instead — the SAME
/// id-reuse defense `computer::shot_dims_for` applies to recorded screenshot
/// geometry. Two registries key off this:
///  - the Ask-card preview registry — so a stale preview can't be attached to a card
///    for a window that merely reused the captured window's number;
///  - the click-focus registry — so a `type`/`key` within the freshness window
///    can't ride a click recorded against a window that closed and had its
///    id reused (under a Full grant that would have meant injecting
///    keystrokes into the replacement with neither a card nor a real click
///    on it).
#[derive(Clone, Debug, PartialEq, Eq)]
struct VerifiedWindowIdentity {
    id: u32,
    app: String,
    title: String,
}

impl VerifiedWindowIdentity {
    fn from_window(w: &computer::WindowInfo) -> Self {
        Self { id: w.id, app: w.app.clone(), title: w.title.clone() }
    }
}

/// Keyed by `(thread, dir, wt)` — the worktree id is part of the key because
/// sibling workers of ONE direction
/// share `(thread, dir)` but are distinct sessions (distinct bearer tokens,
/// distinct lease holders — see `computer::ControlHolder.wt`), so a
/// `(thread, dir)`-keyed entry let worker A's freshly-captured preview attach
/// to worker B's input approval card whenever their resolved windows matched —
/// cross-session context the card's human should never be shown. Same widening
/// `computer::record_shot_dims` has, for the same reason.
fn screenshot_previews(
) -> &'static Mutex<HashMap<(i32, String, Option<i32>), (String, VerifiedWindowIdentity, u64)>> {
    static PREVIEWS: OnceLock<
        Mutex<HashMap<(i32, String, Option<i32>), (String, VerifiedWindowIdentity, u64)>>,
    > = OnceLock::new();
    PREVIEWS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The registry's capacity. Chosen generously
/// above any realistic number of SIMULTANEOUSLY ACTIVE computer-use sessions
/// this process would ever host at once (each entry is one small JPEG
/// thumbnail, refreshed per-session on its own next screenshot — this is not
/// a per-call log, just the latest preview per live session) — no LRU crate
/// needed for a bound this generous; a plain "find the one oldest entry and
/// drop it" scan is more than fast enough at this size.
const MAX_PREVIEWS: usize = 32;

/// Evict the single oldest entry (by insertion timestamp) from `map` if it is
/// already AT capacity — split out from [`store_screenshot_preview`] purely
/// so it's unit-testable against a `HashMap` with explicit, deterministic
/// timestamps: real wall-clock inserts in a tight test loop can land on the
/// SAME millisecond, which would make "the oldest" ambiguous for a test
/// relying on `now_ms()` alone. A no-op when `map` has room, or when the key
/// about to be written already exists (an UPDATE never grows the map, so it
/// never needs to evict anything to make room for itself) — see this
/// function's one caller for that guard.
fn evict_oldest_if_full(
    map: &mut HashMap<(i32, String, Option<i32>), (String, VerifiedWindowIdentity, u64)>,
) {
    if map.len() < MAX_PREVIEWS {
        return;
    }
    if let Some(oldest_key) = map
        .iter()
        .min_by_key(|(_, (_, _, ts))| *ts)
        .map(|(k, _)| k.clone())
    {
        map.remove(&oldest_key);
    }
}

fn store_screenshot_preview(
    thread: i32,
    dir: &str,
    wt: Option<i32>,
    preview: String,
    identity: VerifiedWindowIdentity,
) {
    let mut g = screenshot_previews().lock().unwrap_or_else(|e| e.into_inner());
    let key = (thread, dir.to_string(), wt);
    // Only evict to make room for a genuinely NEW key — refreshing an
    // EXISTING (thread, dir, wt)'s preview (the common case: a session that
    // screenshots repeatedly) must not count against capacity or trigger an
    // eviction of some unrelated session's entry.
    if !g.contains_key(&key) {
        evict_oldest_if_full(&mut g);
    }
    g.insert(key, (preview, identity, now_ms()));
}

/// The most recent screenshot preview (and the window id it came from) for
/// `(thread, dir, wt)`, if any — see [`screenshot_previews`]'s doc. Read only
/// from [`preview_for_action`] within this same module (the
/// server-side gate owns preview attachment; `bus::server::handle_ask` no
/// longer does — see this module's own top doc comment).
fn last_screenshot_preview(
    thread: i32,
    dir: &str,
    wt: Option<i32>,
) -> Option<(String, VerifiedWindowIdentity)> {
    let g = screenshot_previews().lock().unwrap_or_else(|e| e.into_inner());
    g.get(&(thread, dir.to_string(), wt))
        .map(|(preview, identity, _ts)| (preview.clone(), identity.clone()))
}

fn window_arg(args: &Value) -> String {
    args.get("window").and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// The ordered gate every INPUT action (the click family, `type`, `key`,
/// `scroll`, `left_click_drag`, `mouse_move` — never `screenshot`,
/// `list_windows`, `cursor_position`, or `wait`) must clear before it's
/// allowed to touch the backend at all:
/// 1. is a permission card already blocking this (thread, dir) —
///    [`ComputerError::SuspendedPendingAsk`] — see [`check_suspended`];
/// 2. are we going faster than the global input throttle allows —
///    [`ComputerError::RateLimited`] — see [`acquire_and_throttle`];
/// 3. does someone else hold the control lease — [`ComputerError::Busy`] —
///    also [`acquire_and_throttle`], checked AFTER the throttle (see that
///    function's own doc for why the order
///    flipped from the reverse it used to be).
///
/// This single gate is split in two, with the
/// MUTATING half ([`acquire_and_throttle`] — it actually takes the 30s
/// control lease and consumes a throttle slot) to run AFTER every
/// action-specific, purely-argument-shaped check for that action (the window
/// argument's presence, the coordinate/text/scroll shape) has already
/// passed, right before `input_flight_guard`. Before this split, the
/// mutating half ran FIRST, so a malformed call (e.g. a `left_click` missing
/// `coordinate`) still occupied the 30s lease and a throttle slot — and lit
/// the desktop-control banner in Settings — for a call that was always going
/// to be rejected anyway. [`check_suspended`] (the non-mutating half) still
/// runs early, right after `approve`: it's about a DIFFERENT, unrelated ask
/// still waiting on the human (so an agent can't click through/at the
/// permission UI while it's up), and doesn't itself acquire or consume
/// anything, so there's no cost to checking it before argument parsing.
///
/// The window's actual RESOLUTION (and,
/// for the mouse family, the coordinate's mapping against it; for `type`/
/// `key`, the focus-freshness check against it) are NOT purely-argument
/// checks — they depend on the live desktop's current state, which can have
/// changed while this call sat queued on `input_flight_guard` behind another
/// session's in-flight action. Those now run AFTER `input_flight_guard`/the
/// first [`recheck_after_guard`] instead — an advisory preflight against a
/// pre-pacing resolve, then authoritatively inside
/// [`pace_activate_verify_and_inject`]'s paced dispatch closure — see that
/// function's own doc, and each input
/// arm of `run_action`, for the full ordering this section now describes.
///
/// Both halves run AFTER [`approve`] in [`run_action`]'s dispatch, never
/// before: `has_open` here is a completely separate concern from "is THIS
/// call authorized at all", which `approve` alone decides — checking them in
/// the other order would mean an action already suspended behind someone
/// else's card could still open (and block on) its OWN card underneath it.
fn check_suspended(asks: &AskRegistry, thread: i32, dir: &str) -> Result<(), String> {
    if asks.has_open(thread, dir) {
        return Err(ComputerError::SuspendedPendingAsk.to_string());
    }
    Ok(())
}

/// The MUTATING half of the input gate — see [`check_suspended`]'s doc
/// comment for the full ordering rationale. Called ONLY once
/// every fallible, non-mutating check for this specific action has already
/// passed, immediately before the backend call itself: a call that was
/// always going to be rejected for a bad argument never reaches this, so it
/// never occupies the control lease or a throttle slot.
///
/// throttle is now checked BEFORE the control
/// lease is (re)acquired — the reverse of this function's own earlier
/// order. Before this, a same-session input call faster than
/// `computer::THROTTLE_MS` apart still ran `acquire_control` FIRST, which
/// unconditionally RENEWS the 30s sliding-window lease (a live same-holder
/// re-acquire renews `expires_at_ms`, per that function's own doc) — only
/// THEN did `throttle_input` reject it. A caller spamming calls faster than
/// the throttle window — each one individually rejected — kept renewing its
/// own lease on every single rejected attempt, so the control lease (and the
/// "an agent is controlling the desktop" banner/OS-level Escape shortcut it
/// keeps registered) could be held open INDEFINITELY by a loop of calls that
/// never actually got to inject anything. `computer::throttle_input` has no
/// side effect on its OWN `Err` path (it only records `now` on success — see
/// that function's own doc), so checking it FIRST costs nothing when it
/// passes, and means a rejected, rate-limited call no longer touches
/// `acquire_control`/the lease at all.
fn acquire_and_throttle(thread: i32, dir: &str, wt: Option<i32>) -> Result<(), String> {
    // a call that is about
    // to be rejected as `Busy` (a DIFFERENT, still-live session holds the
    // control lease) must NOT consume a throttle slot on the way out. The
    // throttle is process-wide and single-slotted; `throttle_input` records
    // `now` on success, so a foreign session polling roughly every
    // `THROTTLE_MS` would pass the throttle, bump that global timestamp, and
    // only THEN hit `Busy` from `acquire_control` — repeatedly, starving the
    // ACTUAL holder's own paced calls into `RateLimited` even though they are
    // ≥ `THROTTLE_MS` apart. Peek the holder first: only a free lease or one
    // this SAME `(thread, dir)` already holds proceeds to the throttle. This
    // keeps the throttle-first order intact — a same-holder call that is itself rate-
    // limited still rejects at `throttle_input` WITHOUT renewing its lease,
    // because `acquire_control` only runs after the throttle passes.
    //
    // Residual (accepted, not a starvation vector): the peek and the
    // acquire below are two separate lock acquisitions, so if the lease is free
    // at the peek but a foreign holder wins the race to acquire it in the gap,
    // this one call consumes a throttle slot and then still gets `Busy`. That
    // can happen at most ONCE per lease-free window (the very next call peeks
    // the new foreign holder and bails before the throttle), so it can never
    // become the repeatable starvation the peek closes.
    if let Some(holder) = computer::control_state() {
        // `wt` is part of the
        // holder identity, so a SIBLING worker (same `(thread, dir)`, different
        // worktree) peeks as a foreign holder and bails with `Busy` here
        // instead of sliding through as the same holder.
        if !(holder.thread == thread && holder.dir == dir && holder.wt == wt) {
            return Err(ComputerError::Busy { thread: holder.thread, dir: holder.dir }.to_string());
        }
    }
    computer::throttle_input().map_err(|e| e.to_string())?;
    computer::acquire_control(thread, dir, wt).map_err(|e| e.to_string())?;
    Ok(())
}

/// A gate every input branch of [`run_action`] clears
/// immediately after acquiring
/// `computer::input_flight_guard()` (before that branch's own fresh window
/// resolution/coordinate mapping/focus check); its sync-checkable concerns
/// are re-run inside [`pace_activate_verify_and_inject`]'s paced closure
/// right after `activate_target` — see that
/// function's own doc for why a second pass is needed on top of the first.
/// Either pass re-verifies the kill switch AND that the control lease this
/// call took in [`acquire_and_throttle`] is STILL held by THIS EXACT
/// `(thread, dir)`.
///
/// Why this is needed on top of every earlier check: `input_flight_guard` is
/// a single process-wide mutex a SECOND `tools/call` for the SAME session
/// can queue on for as long as the FIRST call's own backend round trip takes
/// (see that function's own doc comment on why it's held for the whole
/// backend call). While a caller sits in that queue, a human can hit Stop —
/// disabling the setting and clearing the control lease
/// (`computer::emergency_stop`) — or, once the lease self-heals on expiry, a
/// completely different `(thread, dir)` could acquire it first. Neither
/// `approve`'s own post-await recheck (which only
/// re-runs once, right after the approval gate, long before the guard is
/// even requested) nor `acquire_and_throttle`'s own `acquire_control` call
/// (which already ran, successfully, before this caller ever started
/// queuing) catch either of those — this is the ONE checkpoint positioned
/// AFTER the queue itself, so a call that waited behind someone else's long
/// hold sees the world as it is NOW, not as it was when it first queued.
///
/// `Ok` requires ALL THREE: no OTHER ask now open for this `(thread, dir)`,
/// [`computer::enabled`] still true, AND
/// [`computer::renew_lease_after_queue`] confirming this EXACT
/// `(thread, dir, wt)` still rightfully holds the lease — judged on the raw
/// monotonic deadline (see that function's doc for why the in-flight
/// liveness override must not apply at this checkpoint) and renewed on
/// success. A DIFFERENT holder, a doomed one, or no holder at all (an
/// expired or force-cleared lease) all fail closed rather than let a call
/// that no longer holds the lease it thinks it does reach the backend
/// anyway.
///
/// this used to check only enabled+lease — but a
/// call queued on `input_flight_guard` can have a
/// BRAND-NEW ask open for its SAME `(thread, dir)` while it waits (a
/// completely unrelated permission request from the same engine, racing in
/// through its own hook), and once such a call finally acquires the guard it
/// went straight to dispatch, exactly the "click through the card" hazard
/// [`check_suspended`]'s own up-front check exists to prevent — just arriving
/// at it from behind the queue instead of before it. `asks.has_open` here can
/// only ever be seeing a DIFFERENT ask than the one THIS call itself went
/// through in [`approve`]: that one is guaranteed resolved (no longer open)
/// by the time `approve` returns, since it only returns once its own
/// `oneshot::Receiver` has actually been answered (or the call was denied/
/// cancelled, which returns `Err` before ever reaching this function at all).
/// Same fail-closed shape and the SAME `ComputerError::SuspendedPendingAsk`
/// text [`check_suspended`] itself returns — from the calling agent's point
/// of view, this is indistinguishable from having queued behind that check
/// in the first place.
async fn recheck_after_guard(
    db: &Db,
    asks: &AskRegistry,
    thread: i32,
    dir: &str,
    wt: Option<i32>,
    auth_gen: u64,
) -> Result<(), String> {
    // The bearer-staleness check first — a request that queued on the flight
    // guard across a relaunch's rotation no longer represents the child that
    // now owns this identity, whatever the lease/asks say.
    verify_bearer_generation(thread, dir, wt, auth_gen)?;
    if asks.has_open(thread, dir) {
        return Err(ComputerError::SuspendedPendingAsk.to_string());
    }
    if !computer::enabled(db).await {
        return Err(ComputerError::Disabled.to_string());
    }
    // revalidate the session
    // at this post-queue checkpoint, immediately before injection — a thread
    // deleted WHILE this call sat queued on `input_flight_guard` (behind another
    // session's in-flight action) must be caught here, not only at the
    // `handle_computer` entry gate. Gated on the revocation set: a thread that
    // was never deleted (every synthetic-identity test, and normal operation)
    // pays only the lock-only lookup, never the `session_is_live` DB check.
    // the request's OWN
    // `wt` is passed through — this used to hardcode `None`, which
    // `session_is_live` reads as "no worktree pinned, direction row is
    // enough": a SESSION-ONLY worker whose worktree a repo delete removed
    // (direction surviving) read as live here merely because the shared
    // direction still existed, and — having acquired a fresh lease AFTER the
    // delete's clear found nothing to clear — sailed through every later
    // lease check to inject under its deleted identity.
    if computer_routes_revoked(thread) && !session_is_live(db, thread, dir, wt).await {
        return Err(SESSION_GONE_MSG.to_string());
    }
    // `renew_lease_after_queue`, not `control_state`: this checkpoint runs
    // with the flight guard ALREADY held, so the in-flight override that
    // `control_state`'s liveness consults is this caller's own flag and
    // would bless the caller's own queue-lapsed (or doomed) lease. The
    // dedicated helper judges the raw monotonic deadline under one lock,
    // renews a still-rightful holder, and compares `wt` too — a SIBLING
    // worker (same `(thread, dir)`) that legitimately took the lease after
    // THIS call's own lease expired while it sat queued is recognized as a
    // different holder, not mistaken for "I still hold it" and waved
    // through to inject.
    match computer::renew_lease_after_queue(thread, dir, wt) {
        computer::LeaseCheckOutcome::Authorized => Ok(()),
        computer::LeaseCheckOutcome::Busy { thread, dir } => {
            Err(ComputerError::Busy { thread, dir }.to_string())
        }
        computer::LeaseCheckOutcome::Lost => Err(
            "the control lease was lost while this call was queued (it may have expired, or been \
             cleared by a kill switch) — retry"
                .to_string(),
        ),
    }
}

/// the purely SYNCHRONOUS kill-switch + control-lease recheck, run INSIDE
/// [`pace_activate_verify_and_inject`]'s paced dispatch closure — on the
/// very blocking-pool thread that is about to call the OS injection backend
/// — at two points: right after the pacing sleep (before the activation
/// side effect), and again right after the authoritative in-closure resolve
/// (immediately before the backend call).
///
/// Every input arm already clears [`recheck_after_guard`] once, after the
/// flight guard. But that runs BEFORE the arm's pre-pacing
/// `resolve_and_verify_target_blocking` — an awaited OS enumeration
/// scheduled onto the blocking pool — before the dispatch closure waits its
/// own turn for a blocking-pool thread, and before the pacing sleep and the
/// in-closure activation/resolve themselves. A human hitting Emergency
/// Stop (which trips [`computer::stop_latched`]) or global Escape (which
/// clears the control lease) DURING any of those, would otherwise go
/// unseen and the click/type/key would still reach their real desktop AFTER
/// the kill switch fired. Reading the stop latch and the control-lease holder
/// are both lock-only, no `.await`/no db, so this can run at the last possible
/// instant on the same thread as the backend call, closing that residual gap
/// completely. Callers `?` this and never fall
/// through to activation/the backend call on an `Err`.
fn recheck_stop_and_lease_before_backend(
    thread: i32,
    dir: &str,
    wt: Option<i32>,
    auth_gen: u64,
) -> Result<(), String> {
    if computer::stop_latched() {
        return Err(ComputerError::Disabled.to_string());
    }
    // The bearer-staleness check rides every one of this helper's call
    // points too (lock-only, like the rest): the pacing sleep, activation,
    // and the in-closure resolve are exactly the kind of long gaps a
    // relaunch's rotation can land inside — see `verify_bearer_generation`.
    verify_bearer_generation(thread, dir, wt, auth_gen)?;
    // The "session deleted after recheck_after_guard, while the final
    // resolve/injection was queued" gap is closed at ITS ROOT rather than
    // here — the delete paths CLEAR the control lease for any route they
    // tear down (see `commands`' `clear_control_if_doomed`), so the lease
    // check below already fails closed for a deleted route. A blanket
    // revocation check HERE would be wrong: this helper is `dir`-blind at
    // the thread level, and `delete_repo` revokes a SURVIVING thread (only
    // one of its directions is gone), so a thread-level refuse would
    // permanently break computer-use for that thread's OTHER directions.
    // Lease-clearing is direction-precise (the lease names exactly one
    // `(thread, dir)`), so it refuses only the torn-down route.
    //
    // `lease_check_for_injection` (not `control_state`): a clear that lands
    // while an injection is in flight leaves the holder VISIBLE for the Stop
    // surfaces but marks it doomed — `control_state` still reports it, and
    // matching on that snapshot would wave a torn-down route straight into
    // the backend. The dedicated check reads holder + doomed under one lock
    // and refuses a doomed holder as Lost.
    match computer::lease_check_for_injection(thread, dir, wt) {
        computer::LeaseCheckOutcome::Authorized => Ok(()),
        computer::LeaseCheckOutcome::Busy { thread, dir } => {
            Err(ComputerError::Busy { thread, dir }.to_string())
        }
        computer::LeaseCheckOutcome::Lost => Err(
            "the control lease was lost just before injection (it may have expired, or been \
             cleared by a kill switch) — retry"
                .to_string(),
        ),
    }
}

/// The shared post-queue tail every input arm of [`run_action`] dispatches
/// through: ONE blocking closure that paces, re-guards, RECLAIMS the
/// approved target, re-resolves/re-verifies it fresh, and only then runs the
/// arm's own `tail` (coordinate mapping / focus-freshness check plus the
/// actual backend injection) — all back-to-back on the SAME blocking-pool
/// thread, with no await point, queue wait, or sleep between any of those
/// steps and the backend call.
///
/// Why one closure: the pacing sleep (`computer::pace_backend_dispatch`)
/// and the blocking-pool queue wait are both real wall-clock gaps. When
/// activation, the authoritative resolve, and the coordinate mapping ran
/// BEFORE them, the target could move, close, lose the foreground, or be
/// replaced (its id reused) DURING those gaps — and the backend call would
/// still inject at the stale coordinates, or into whatever now owns them,
/// with only a stop/lease recheck (which never looks at the TARGET) in
/// between. Running the whole target-facing sequence AFTER the pacing sleep
/// closes that: what gets activated/verified/mapped is the desktop as it is
/// immediately before injection, not as it was before the sleep. No extra
/// concurrency cap is needed for the OS work in here: `input_flight_guard`,
/// held by every calling arm across this entire call, already serializes
/// the process to one in-flight input action at a time.
///
/// In-closure order, each step fail-closed (`?`):
///  1. `computer::pace_backend_dispatch()` — sleep out the pacing gap
///     FIRST, so everything below sees the post-sleep world.
///  2. [`recheck_stop_and_lease_before_backend`] — a kill switch tripped
///     during the sleep (or the queue wait before it) is honored before the
///     activation SIDE EFFECT below, not merely before the injection.
///  3. `asks.has_open` — BEFORE the activation side effect below, not
///     after: raising/focusing a window while a Needs-you card is open can
///     throw that window over the card the human is being asked to read.
///     A card can open during the pacing sleep or the blocking-pool queue,
///     both of which land after the caller's own [`recheck_after_guard`].
///  4. [`activate_target`] on `activation_target` (the arm's pre-pacing
///     resolve) — reclaim the foreground for the target. Activation itself
///     re-verifies the full identity against the live desktop (see its
///     doc), so a target replaced during the sleep fails closed here rather
///     than raising the replacement.
///  5. [`computer::renew_lease_after_queue`] — activation is a slow OS
///     shell-out a Stop/Escape can land during: stop latch +
///     doomed/foreign/expired holder, renewing a still-rightful holder
///     whose lease lapsed during the wait, exactly like the post-
///     flight-guard checkpoint. The `enabled`-flag and route-revocation
///     halves of [`recheck_after_guard`] are async (db reads) and cannot run
///     on this thread — deliberately fine: every disable path trips the
///     stop latch and every route-teardown path clears the lease (see
///     [`recheck_stop_and_lease_before_backend`]'s own doc), so the
///     sync checks here already fail closed for both.
///  6. [`resolve_and_verify_target`] — the AUTHORITATIVE fresh resolve the
///     arm's `tail` maps/checks against, recorded into `window_id_out`
///     (even on a verify failure, so the audit line still names the window
///     that was targeted) and verified byte-for-byte against `approved`.
///  7. [`recheck_stop_and_lease_before_backend`] once more — the resolve
///     above is itself an OS enumeration; this keeps the final stop/lease
///     read at the last possible instant before the backend call.
///  8. `asks.has_open` a FINAL time — the resolve at step 6 is a blocking
///     enumeration a brand-new card can open during, and no injection may
///     land while one is pending (`check_suspended`'s rule).
///  9. `tail(&fresh)` — the arm's own coordinate mapping (pure in-memory
///     lookups) or focus-freshness check, then the backend injection,
///     against the fresh resolve ONLY — never the pre-pacing snapshot.
///
/// Returns the tail's value together with the fresh [`computer::WindowInfo`]
/// it ran against, so arms can name the actually-injected window in their
/// result text (and the click family can seed focus-freshness for it).
async fn pace_activate_verify_and_inject<T: Send + 'static>(
    asks: &AskRegistry,
    thread: i32,
    dir: &str,
    wt: Option<i32>,
    auth_gen: u64,
    window_query: &str,
    approved: &Option<ApprovedWindow>,
    activation_target: computer::WindowInfo,
    window_id_out: &mut Option<u32>,
    tail: impl FnOnce(&computer::WindowInfo) -> Result<T, String> + Send + 'static,
) -> Result<(T, computer::WindowInfo), String> {
    let asks = asks.clone();
    let dir_owned = dir.to_string();
    let window_query = window_query.to_string();
    let approved = approved.clone();
    // `window_id_out` is a reference into the caller's stack frame, which
    // can't cross into a `'static` closure — same local-then-write-back
    // shape as `resolve_and_verify_target_blocking`, preserving the
    // "record the id even when verification fails" semantics.
    let (id, result) = on_blocking(move || {
        computer::pace_backend_dispatch();
        let mut id = None;
        let result = (|| {
            recheck_stop_and_lease_before_backend(thread, &dir_owned, wt, auth_gen)?;
            // BEFORE activation, not after: `activate_target` is itself a
            // desktop side effect — it RAISES and FOCUSES the target
            // application — so running it while a Needs-you card is open can
            // throw that window over the very card the human is being asked
            // to read, and can pull focus away from it. A card can open
            // during the pacing sleep above or while this closure sat queued
            // for a blocking thread, both of which land after the caller's
            // own `recheck_after_guard`. `check_suspended`'s rule ("no
            // desktop-facing action while a card is pending") therefore has
            // to be re-checked here, ahead of the side effect, exactly like
            // the stop/lease recheck immediately above it. Lock-only, so it
            // costs nothing to check at the last instant.
            if asks.has_open(thread, &dir_owned) {
                return Err(ComputerError::SuspendedPendingAsk.to_string());
            }
            activate_target(&activation_target)?;
            match computer::renew_lease_after_queue(thread, &dir_owned, wt) {
                computer::LeaseCheckOutcome::Authorized => {}
                computer::LeaseCheckOutcome::Busy { thread, dir } => {
                    return Err(ComputerError::Busy { thread, dir }.to_string());
                }
                computer::LeaseCheckOutcome::Lost => {
                    return Err(
                        "the control lease was lost during target activation (it may have \
                         expired, or been cleared by a kill switch) — retry"
                            .to_string(),
                    );
                }
            }
            let fresh = resolve_and_verify_target(&window_query, &approved, &mut id)?;
            recheck_stop_and_lease_before_backend(thread, &dir_owned, wt, auth_gen)?;
            // one FINAL pending-ask check, immediately before `tail` runs the
            // backend injection. The `has_open` check above ran BEFORE
            // `resolve_and_verify_target`, whose window enumeration is itself a
            // blocking OS call — a brand-new Needs-you card for this same
            // `(thread, dir)` (an unrelated permission request racing in
            // through the engine's hook) can open DURING that enumeration, and
            // without this check `tail` would click/type/scroll while that card
            // is still on screen: exactly the "inject through the card" hazard
            // `check_suspended`/`recheck_after_guard` guard everywhere else.
            // Cheap (one lock-only lookup) and the last thing before the input.
            if asks.has_open(thread, &dir_owned) {
                return Err(ComputerError::SuspendedPendingAsk.to_string());
            }
            let out = tail(&fresh)?;
            Ok((out, fresh))
        })();
        (id, result)
    })
    .await?;
    *window_id_out = id;
    result
}

/// `arr[0]`/`arr[1]` must each fit `u32` — this
/// used to read each as `i64` then cast `as u32`, which silently WRAPS an
/// in-range-for-i64-but->u32::MAX value (e.g. `4294967296` — `2^32` — casts
/// straight to `0`), letting an absurd JSON integer sail past the "must be
/// non-negative" filter and land on a real, in-bounds-looking coordinate
/// instead of being rejected. `u32::try_from` fails (rather than wrapping)
/// for anything outside `0..=u32::MAX`, including negatives, so the single
/// `.and_then` below both replaces the old separate `>= 0` filter and closes
/// the overflow hole with the SAME existing error text — no behavior change
/// for any value that was already being accepted or rejected correctly.
fn parse_coordinate(args: &Value, key: &str) -> Result<(u32, u32), String> {
    let Some(arr) = args.get(key).and_then(|v| v.as_array()) else {
        return Err(format!("missing required '{key}': [x, y]"));
    };
    if arr.len() != 2 {
        return Err(format!("'{key}' must be exactly [x, y]"));
    }
    let x = arr[0].as_i64().and_then(|v| u32::try_from(v).ok());
    let y = arr[1].as_i64().and_then(|v| u32::try_from(v).ok());
    match (x, y) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(format!("'{key}' must be two non-negative integers [x, y]")),
    }
}

fn required_text(args: &Value) -> Result<&str, String> {
    args.get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required 'text'".to_string())
}

/// Hard ceiling on `type`'s payload length
/// : a single approved `type` call is meant to be a bounded UI
/// interaction — a form field, a search box, a short reply — not an
/// HTTP-body-sized blob. Under a Full/Always grant, `enigo.type_text` runs
/// completely SYNCHRONOUSLY inside [`computer::input_flight_guard`] — held
/// for this whole call, per that function's own doc — with no way to CANCEL
/// an in-flight injection partway through: an unbounded string could keep
/// injecting well past `CONTROL_LEASE_MS` (the 30s control lease this same
/// call already holds), well past however long the OS-level global Escape
/// shortcut takes to register/unregister around a lease
/// (`computer::sync_shortcut_state`), and well past anything else this
/// module's other fixed-duration gates implicitly assume a single input
/// action finishes within.
///
/// `MAX_TYPE_CHARS` is a DETERMINISTIC bound, not a full fix: chunking a
/// long `type` into multiple shorter injections with a lease renewal between
/// chunks (so the FIRST chunk's own cancellation window would actually cover
/// the rest) is the more complete answer, but it needs a change at the
/// `enigo` layer this change doesn't touch (there is no partial/cancellable
/// `type_text` this module could call into today) — tracked as a follow-up,
/// not required to close the bound this ceiling already provides
/// unconditionally. `5000` chars is generous for any single interactive
/// field a human would plausibly want an agent to fill in one call, while
/// comfortably ruling out something HTTP-body-shaped.
///
/// `#[doc(hidden)] pub` (not `pub(crate)`): `tests/computer_mcp.rs` is a
/// separate integration-test crate that needs the EXACT cap this module
/// enforces to build an over-limit payload, rather than hardcoding a
/// duplicate literal that could silently drift from this one — mirrors
/// `args_digest`'s own doc comment on why a test-only-consumed constant is
/// `#[doc(hidden)] pub` here rather than `#[cfg(test)]`.
#[doc(hidden)]
pub const MAX_TYPE_CHARS: usize = 5_000;

/// Reject a `type` payload over [`MAX_TYPE_CHARS`] — checked right after
/// [`required_text`] and before the control lease/throttle are ever touched
/// (the same "fail on a bad argument before it costs anything" ordering
/// discipline every other action's argument validation already follows —
/// see `check_suspended`'s own doc comment), and well before
/// `backend::ComputerBackend::type_text` ever sees the string.
fn check_type_length(text: &str) -> Result<(), String> {
    let len = text.chars().count();
    if len > MAX_TYPE_CHARS {
        return Err(format!(
            "text is too long: {len} characters (max {MAX_TYPE_CHARS}) — split it into multiple \
             shorter type calls, or shorten it"
        ));
    }
    Ok(())
}

/// `scroll_direction` (required) + `scroll_amount` (optional, default 3,
/// capped at 30 — never rejected for being too large, just clamped; a
/// NEGATIVE amount is rejected outright, see below) into a `(dx, dy)` delta
/// `backend::ComputerBackend::scroll` understands.
fn parse_scroll(args: &Value) -> Result<(i32, i32), String> {
    let direction = args.get("scroll_direction").and_then(|v| v.as_str()).unwrap_or("");
    // distinguish an
    // ABSENT `scroll_amount` (→ the documented default of 3) from one that is
    // PRESENT but not representable as an `i64` (a JSON string like `"30"`, or a
    // number outside `i64`'s range). The old `.and_then(as_i64).unwrap_or(3)`
    // collapsed both into 3, so a malformed value passed `pure_validate`, the
    // approval card rendered the raw requested value, and the backend then
    // scrolled a DIFFERENT distance after the human allowed it. A present-but-
    // invalid value is now rejected outright, before approval.
    let amount = match args.get("scroll_amount") {
        None | Some(Value::Null) => 3,
        Some(v) => match v.as_i64() {
            Some(n) => n,
            None => {
                return Err(
                    "'scroll_amount' must be an integer (a plain JSON number); omit it to use the \
                     default of 3"
                        .to_string(),
                )
            }
        },
    };
    // A negative amount is rejected, not clamped: direction already lives in
    // `scroll_direction`, so a negative number is at best a sign confusion —
    // and silently clamping it to 0 turned the call into a successful no-op
    // that still activated the target and moved the cursor, while the human
    // approved a card showing the raw negative value. The requested and
    // executed operations must match, so this fails in `pure_validate`,
    // before any card ever opens. The HIGH end keeps the documented
    // clamp-at-30 (a too-large scroll is the same gesture, just shorter).
    if amount < 0 {
        return Err(
            "'scroll_amount' must not be negative — use 'scroll_direction' to choose the \
             direction"
                .to_string(),
        );
    }
    let amount = amount.min(30) as i32;
    match direction {
        "up" => Ok((0, -amount)),
        "down" => Ok((0, amount)),
        "left" => Ok((-amount, 0)),
        "right" => Ok((amount, 0)),
        other => Err(format!(
            "'scroll_direction' must be one of up|down|left|right, got '{other}'"
        )),
    }
}

/// `duration_ms` (required) for `action=wait`, capped at 5000 rather than
/// rejected when the agent asks for longer — matches `scroll_amount`'s
/// clamp-not-reject policy above, so the two capped numeric params in this
/// tool behave the same way.
fn parse_duration_ms(args: &Value) -> Result<u64, String> {
    match args.get("duration_ms").and_then(|v| v.as_u64()) {
        Some(ms) => Ok(ms.min(5000)),
        None => Err("missing required 'duration_ms'".to_string()),
    }
}

/// Every pure, side-effect-free schema check for `action`'s arguments,
/// factored out so [`run_action`] can run them ALL before the approval gate
/// (`approve`) ever opens a Needs-you card. A malformed call is rejected here, up front — never
/// after a human answered a card for it, which would mint a standing
/// Always/Full grant for a request that then fails its own arm's parse and
/// never executes. This mirrors, EXACTLY, the pure checks each arm in
/// `run_action`'s `match` already does; the arms keep doing them (idempotent)
/// and additionally do the LIVE checks — window resolution, focus freshness,
/// the control lease — that can only run after approval. An unknown `action`
/// is rejected here too, so it can never reach `approve` and open a card
/// either.
fn pure_validate(action: &str, args: &Value) -> Result<(), String> {
    if !VALID_ACTIONS.iter().any(|a| *a == action) {
        return Err(unknown_action_error(action));
    }
    // reject EVERY
    // argument outside the action's closed allowlist — see
    // [`reject_unknown_args`]. Generalizes (and replaces) the earlier
    // windowless-`window` and non-typing-`text` one-offs: those
    // closed two named fields, but the tool schema permits additional
    // properties, so ANY unrecognized key (`password`, …) could still ride a
    // schema-valid request onto the approval card and into the durable audit
    // verbatim.
    reject_unknown_args(action, args)?;
    match action {
        "left_click" | "right_click" | "double_click" | "triple_click" | "mouse_move" => {
            required_window(args)?;
            parse_coordinate(args, "coordinate")?;
        }
        "left_click_drag" => {
            required_window(args)?;
            parse_coordinate(args, "start_coordinate")?;
            parse_coordinate(args, "coordinate")?;
        }
        "scroll" => {
            required_window(args)?;
            parse_coordinate(args, "coordinate")?;
            parse_scroll(args)?;
        }
        "type" => {
            required_window(args)?;
            check_type_length(required_text(args)?)?;
        }
        "key" => {
            required_window(args)?;
            let tokens = computer::parse_key_combo(required_text(args)?).map_err(|e| e.to_string())?;
            reject_unsafe_key_combo(&tokens)?;
        }
        "screenshot" => {
            required_window(args)?;
        }
        "wait" => {
            parse_duration_ms(args)?;
        }
        // list_windows / cursor_position: nothing to parse — the allowlist
        // check above already rejected everything but `action` itself.
        _ => {}
    }
    Ok(())
}

/// The closed per-action argument allowlist — exactly the keys each dispatch arm actually
/// consumes, plus the discriminant `action` itself. One table serving BOTH
/// [`reject_unknown_args`] (request-time rejection) and
/// [`redact_audit_args`] (audit-time stripping), so the two boundaries can
/// never disagree about what an action legitimately carries.
fn allowed_args(action: &str) -> &'static [&'static str] {
    match action {
        "left_click" | "right_click" | "double_click" | "triple_click" | "mouse_move" => {
            &["action", "window", "coordinate"]
        }
        "left_click_drag" => &["action", "window", "start_coordinate", "coordinate"],
        "scroll" => &["action", "window", "coordinate", "scroll_direction", "scroll_amount"],
        "type" | "key" => &["action", "window", "text"],
        "screenshot" => &["action", "window"],
        "wait" => &["action", "duration_ms"],
        // list_windows / cursor_position take nothing beyond the action.
        _ => &["action"],
    }
}

/// reject any argument
/// outside [`allowed_args`], BEFORE a card is built or anything is logged.
/// The tool schema permits additional properties, so a schema-valid request
/// could smuggle arbitrary content under an unrecognized key (`password`,
/// …) — ignored by dispatch, but carried verbatim onto the approval card and
/// into the durable audit. Generalizes the earlier windowless-`window` and
/// non-typing-`text` rules: unconsumed fields are rejected as a
/// class, not one named field at a time.
///
/// The offending KEY is deliberately NOT echoed in the error: the error
/// string becomes the call's audited outcome, and a key name is as
/// attacker-chosen as a value — echoing it would persist the very content
/// this rejection exists to keep out of the log.
fn reject_unknown_args(action: &str, args: &Value) -> Result<(), String> {
    let allowed = allowed_args(action);
    let Some(obj) = args.as_object() else {
        // Non-object args carry no smuggleable keys; each arm's own parse
        // rejects the malformed shape with its specific message.
        return Ok(());
    };
    if obj.keys().any(|key| !allowed.iter().any(|a| a == key)) {
        return Err(format!(
            "'{action}' was given an argument it does not take — allowed arguments: {}",
            allowed.join(", ")
        ));
    }
    Ok(())
}

/// reject two `key`
/// payloads outright, from [`pure_validate`] — BEFORE the approval card is ever
/// built (so neither reaches the IM bridge) and before any backend work:
///
///  - a BARE printable character (a lone `Unicode` token, no modifier): this is
///    char-by-char TEXT entry, which `type` — not `key` — exists for. Unlike
///    `type`, a `key` payload is NOT redacted on the outbound Lark card or in
///    the durable audit, so routing sensitive text through `key` one character
///    at a time would disclose each character; forcing it onto `type` (which
///    redacts) closes that.
///  - a BARE `Escape` (no modifier): the process-wide global Escape shortcut is
///    the kill switch's OS-level layer whenever a control lease is held, so an
///    injected bare Escape can be swallowed as Emergency Stop instead of
///    reaching the target window — disabling Computer Use rather than acting on
///    it. A MODIFIED chord (e.g. `shift+escape`)
///    does not match the bare-Escape shortcut and is deliberately left alone.
fn reject_unsafe_key_combo(tokens: &[computer::KeyToken]) -> Result<(), String> {
    // Every SEMANTICALLY shift-only printable
    // chord is the same char-by-char TEXT entry as a bare printable key —
    // judged by [`is_printable_text_chord`], not by exact slice patterns,
    // which kept missing shapes one variation away (`shift+h` slipped past a
    // bare-`h` pattern, then `shift+shift+h` slipped past the `shift+h`
    // pattern). See that predicate's own doc.
    if is_printable_text_chord(tokens) {
        return Err(
            "send printable characters with the `type` action, not `key` — `key` is for \
             named keys and modifier shortcuts (e.g. `enter`, `tab`, `ctrl+c`)"
                .to_string(),
        );
    }
    // Reject any chord that would inject an UNMODIFIED
    // Escape press — judged semantically by [`chord_injects_bare_escape`],
    // not by the exact bare-`escape` slice, which the same duplicate-token
    // dodge closed for printable chords slipped past
    // (`escape+escape` pressed-and-held a bare Escape while missing
    // `[Named(Escape)]`).
    if chord_injects_bare_escape(tokens) {
        return Err(
            "`escape` can't be injected through the `key` action unless modified (e.g. \
             `shift+escape`) — an unmodified Escape press collides with weft's global \
             emergency-stop shortcut and could trip the kill switch instead of reaching the \
             target window"
                .to_string(),
        );
    }
    Ok(())
}

/// would executing this chord
/// press Escape with NO real modifier held — the exact event the OS-level
/// global emergency-stop shortcut is registered for? On global-hotkey
/// backends that observe synthesized input, such an injected press is
/// swallowed as Emergency Stop (tripping the latch, cancelling GUI asks,
/// disabling Computer Use) instead of reaching the target window — a
/// self-DoS the agent can also fire deliberately.
///
/// The executor holds every token but the last, in order, then clicks the
/// last (see `os.rs`'s `key`). So:
///  - an Escape anywhere in the HELD prefix is pressed while, at best, only
///    EARLIER prefix tokens are down — ordering is caller-controlled, so any
///    held Escape fails closed (`escape+escape`, `escape+a`);
///  - Escape as the CLICKED key is modified only if at least one REAL
///    modifier (meta/ctrl/alt/shift) is among the held tokens — a held
///    NON-modifier (`tab+escape`) leaves the modifier state empty, so the
///    bare-Escape hotkey still matches. `shift+escape` / `ctrl+escape` (the
///    deliberately-allowed modified chords) stay accepted.
fn chord_injects_bare_escape(tokens: &[computer::KeyToken]) -> bool {
    let is_escape = |t: &computer::KeyToken| {
        matches!(t, computer::KeyToken::Named(computer::NamedKey::Escape))
    };
    let is_real_modifier = |t: &computer::KeyToken| {
        matches!(
            t,
            computer::KeyToken::Meta
                | computer::KeyToken::Control
                | computer::KeyToken::Alt
                | computer::KeyToken::Shift
        )
    };
    let Some((last, held)) = tokens.split_last() else {
        return false;
    };
    if held.iter().any(is_escape) {
        return true;
    }
    is_escape(last) && !held.iter().any(is_real_modifier)
}

/// the SEMANTIC
/// "printable text entry" test the earlier/31 findings converge on. True
/// when the chord consists of NOTHING but Shift modifiers and at least one
/// printable `Unicode` token — `h`, `shift+h`, `shift+shift+h`, any
/// duplicate/reordered variant: every shape whose OS effect is "hold (only)
/// Shift, click printable keys", i.e. TEXT entry, which `type` (redacted
/// end-to-end) exists for. A predicate over the token multiset cannot be
/// dodged by repeating or reordering tokens the way the previous exact slice
/// patterns could. Any non-Shift modifier (ctrl/alt/meta) or any named key
/// makes the chord a COMMAND and exempts it — commands are not content, and
/// keeping them readable in the audit is deliberate forensics.
/// Shared verbatim by [`reject_unsafe_key_combo`] (the request-time reject)
/// and [`redact_audit_args`] (the audit-time redaction), so the two
/// boundaries can never disagree on what counts as text entry.
fn is_printable_text_chord(tokens: &[computer::KeyToken]) -> bool {
    let mut printable = 0usize;
    for token in tokens {
        match token {
            computer::KeyToken::Shift => {}
            computer::KeyToken::Unicode(_) => printable += 1,
            _ => return false,
        }
    }
    printable >= 1
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One `.weft/computer-audit.jsonl` line — see [`append_audit`].
#[derive(serde::Serialize)]
struct AuditEntry<'a> {
    ts_ms: u64,
    thread: i32,
    dir: &'a str,
    action: &'a str,
    window_query: &'a str,
    window_id: Option<u32>,
    args: &'a Value,
    outcome: &'a str,
}

/// Pure JSON-line rendering, split out from [`append_audit`] specifically so
/// it's unit-testable without touching the filesystem or the DB — see the
/// `tests` module below.
fn audit_line(entry: &AuditEntry<'_>) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(entry)?;
    line.push('\n');
    Ok(line)
}

/// Append one audit line for EVERY `tools/call` (every action, success or
/// failure) to the session's own `.weft/computer-audit.jsonl` — best-effort:
/// a resolution or write failure here (including a refused symlink — see
/// [`refuse_symlinks`], or an `O_NOFOLLOW`-refused leaf — see
/// [`open_audit_file_for_append`]) never affects the actual tool result, it
/// just means this one call goes unlogged.
async fn append_audit(db: &Db, thread: i32, dir: &str, wt: Option<i32>, entry: &AuditEntry<'_>) {
    let Some(path) = audit_log_path(db, thread, dir, wt).await else {
        return;
    };
    // recheck the
    // direction-precise route revocation immediately before creating/writing
    // the path. `audit_log_path` can resolve while the session's rows still
    // exist, a concurrent delete can then remove the whole computer-output
    // subtree, and the `create_dir_all` below would RECREATE it for a deleted
    // session — output regained after cleanup. The delete paths publish their
    // revocation BEFORE the destructive cascade, so an append that
    // reaches this check after deletion began is refused here; the lead lane
    // is covered too (`RouteRevocation::Whole` matches every lane). Best-effort
    // like the rest of this function — a refused line just goes unlogged.
    //
    // that recheck alone
    // was still check-then-act across `.await`s — a delete could publish its
    // revocation AND run `remove_computer_output_*` entirely BETWEEN this
    // check passing and the write below landing, recreating the subtree for
    // the deleted session anyway. Hold [`revocation_txn_lock`] across the
    // whole check → create_dir_all → write sequence: every delete flow holds
    // that same lock from BEFORE it publishes revocation until AFTER its
    // output removal (see `commands::delete_*_after_fence`), so either this
    // append finishes first (and the delete's later removal sweeps the line
    // away) or the delete finishes first (and the check here sees the
    // revocation and refuses). Lock order is `revocation_txn_lock` →
    // `audit_write_lock` (inside `write_audit_line_locked`), and the delete
    // flows never take `audit_write_lock` at all, so no cycle exists.
    let _revocation_txn = revocation_txn_lock().lock().await;
    if route_revoked_sync(thread, dir, wt) {
        return;
    }
    let Some(parent) = path.parent() else { return };
    if tokio::fs::create_dir_all(parent).await.is_err() {
        return;
    }
    let Ok(line) = audit_line(entry) else { return };
    write_audit_line_locked(&path, &line).await;
}

/// process-wide async lock serializing every
/// "check size → (maybe) rotate → open → append" sequence
/// [`write_audit_line_locked`] performs — the concurrent-writer race this
/// closes: two `tools/call`s finishing at nearly the same moment each
/// independently read the file's size (via [`rotate_audit_if_needed`]) as
/// still under [`MAX_AUDIT_BYTES`] — true at the instant EACH checked, since
/// neither call's check-then-act was synchronized against the other — so
/// BOTH skip rotation and both append; worse, both could decide TO rotate,
/// racing each other's `rename` and losing whichever line landed between
/// the loser's stale size-check and its own now-superseded open. The
/// aggregate can land arbitrarily far past the cap, or a rotation can
/// clobber/lose lines, before any single caller's own check would have
/// caught it.
///
/// A single GLOBAL lock (not keyed per-path) is deliberate: audit logging is
/// best-effort and each write is small (one JSON line), so there is no
/// throughput concern serializing every session's own audit append behind
/// one mutex — and a single lock can never deadlock across two DIFFERENT
/// sessions' paths the way a per-path lock keyed by a fallible hash/pool
/// could. `tokio::sync::Mutex` (not `std::sync::Mutex`): held ACROSS the
/// `.await`s inside [`write_audit_line_locked`] (the file open/write), which
/// a `std::sync::Mutex` guard cannot survive across an `.await` point at
/// all — `tokio::sync::Mutex::lock` never poisons (unlike `std::sync::
/// Mutex`, there is no fallible `.unwrap_or_else(|e| e.into_inner())` needed
/// here).
fn audit_write_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// The actual critical section [`append_audit`] serializes on
/// — split out from `append_audit` so a test can drive
/// genuine concurrent calls against a raw path directly, without needing a
/// `Db`/workspace resolution just to reach the race this closes. Holds
/// [`audit_write_lock`] across the WHOLE size-check → rotate → open →
/// append sequence, so two concurrent callers can never interleave their
/// own rotate decisions or race an open against a rotation still in
/// flight — see that lock's own doc for the exact corruption this
/// prevents. Best-effort like every other step here: never blocks or fails
/// the actual tool call; a lock/rotate/open/write failure here just means
/// this one line goes unlogged.
async fn write_audit_line_locked(path: &std::path::Path, line: &str) {
    use tokio::io::AsyncWriteExt;
    let _guard = audit_write_lock().lock().await;
    // rotate BEFORE opening for append — see
    // `rotate_audit_if_needed`'s own doc for the unbounded-growth hazard
    // this closes (a Full/exact-Always-granted agent looping
    // `cursor_position`/`list_windows` with no throttle of its own would
    // otherwise append forever).
    rotate_audit_if_needed(path);
    let Ok(mut file) = open_audit_file_for_append(path).await else {
        return;
    };
    if file.write_all(line.as_bytes()).await.is_ok() {
        // `tokio::fs::File` buffers writes and does NOT flush on drop:
        // dropping the handle right after `write_all` (as the guard releases
        // at the end of this function) can lose the just-written line when its
        // background write op hasn't completed yet — an intermittent
        // single-line loss under concurrent appends (observed as a flaky
        // failure of `concurrent_appends_serialize_through_rotation_...`).
        // Flush inside the critical section so every audit line is durable
        // before the lock releases and the next writer's rotate/size-check
        // runs against the fully-written file.
        let _ = file.flush().await;
    }
}

/// Single-file rotation cap for `computer-audit.jsonl`
/// : unlike screenshots ([`prune_old_screenshots`]/
/// [`MAX_RETAINED_SCREENSHOTS`] equivalents live in `computer` — see
/// `computer::screenshot_window`'s own retention cap), the audit log had NO
/// rotation/quota/cleanup at all — every `tools/call`, Full/exact-Always
/// granted or not, appends one more line forever. A looping
/// `cursor_position`/`list_windows` agent (both `ReadOnly`, both throttled
/// only by the general input rate limit which doesn't even apply to
/// observation actions) could grow this file without bound until the
/// human's disk fills up.
const MAX_AUDIT_BYTES: u64 = 5 * 1024 * 1024;

/// Rotate `path` — the audit log's own file — to `<path>.1` (overwriting
/// whatever was previously there) if it is already at/over
/// [`MAX_AUDIT_BYTES`], via [`rotate_audit_at_size`]. Total on-disk usage for
/// this one file stays bounded to roughly `2 * MAX_AUDIT_BYTES`: the live
/// file plus, at most, one rotated `.1`. A more elaborate multi-generation
/// rotation, or a per-session quota, is a follow-up — this change ships the
/// simplest scheme that actually bounds growth.
fn rotate_audit_if_needed(path: &std::path::Path) {
    rotate_audit_at_size(path, MAX_AUDIT_BYTES);
}

/// The actual rotation logic, parameterized on `max_bytes`
/// so a test can drive it against a real, small file without
/// having to write out multiple real megabytes just to exercise the
/// threshold — [`rotate_audit_if_needed`] is the one production caller,
/// always with [`MAX_AUDIT_BYTES`].
///
/// Symlink safety: `path` has ALREADY been walked component-by-component by
/// [`refuse_symlinks`] (via [`audit_log_path`]) before [`append_audit`] ever
/// calls this — but a worktree is repository-controlled content, so the
/// window between that check and THIS call is still open. Re-checks via
/// `symlink_metadata` (never plain `metadata`) right here, immediately
/// before the `rename`: if `path` is no longer a REGULAR file at all (a
/// symlink swapped into place in that gap) rotation is skipped outright
/// rather than renaming something that may not mean what the size check
/// assumed; the `.1` destination gets the identical check before anything is
/// renamed onto it, so this never renames a legitimate file over — or reads
/// the size of — a symlink planted at either path.
///
/// Best-effort, like every other step [`append_audit`] takes: ANY failure
/// here (a metadata read, the rename itself) silently skips rotation for
/// this one call — audit logging is best-effort end to end, and a rotation
/// hiccup must never turn into a dropped/corrupted audit line, let alone a
/// failed tool call.
fn rotate_audit_at_size(path: &std::path::Path, max_bytes: u64) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return; // doesn't exist yet, or unreadable — nothing to rotate
    };
    if !meta.file_type().is_file() || meta.len() < max_bytes {
        return;
    }
    let mut rotated_name = path.as_os_str().to_os_string();
    rotated_name.push(".1");
    let rotated = std::path::PathBuf::from(rotated_name);
    if let Ok(rotated_meta) = std::fs::symlink_metadata(&rotated) {
        if rotated_meta.file_type().is_symlink() {
            return; // never rename onto a symlinked destination
        }
        // remove the
        // previous generation EXPLICITLY before the rename. On unix, `rename`
        // replaces an existing destination atomically — but on Windows it
        // FAILS instead, and since rotation is best-effort (the error was
        // ignored), every rotation after the first silently no-op'd and the
        // live log grew without bound. Removing first is portable; the cost is
        // giving up unix's atomic replace, which is fine here: the whole
        // check→rotate→append sequence is already serialized under
        // [`audit_write_lock`], so no concurrent writer can observe (or race)
        // the tiny remove→rename gap, and a crash inside it merely loses the
        // OLD generation — the live log survives, which is the right
        // best-effort trade for an audit trail. The symlink check above stays:
        // a symlink at the destination is never removed OR renamed over.
        let _ = std::fs::remove_file(&rotated);
    }
    let _ = std::fs::rename(path, &rotated);
}

/// The actual `open(2)` call [`append_audit`] makes — factored out so its own
/// `O_NOFOLLOW` defense is unit-testable in
/// isolation from [`refuse_symlinks`]'s EARLIER, per-component check.
///
/// [`refuse_symlinks`] (via [`audit_log_path`]) already refuses a symlink
/// that's ALREADY sitting at any path component — including the leaf file
/// itself — by the time it runs its `symlink_metadata` check. What it can
/// NOT close is the window AFTER that check returns and BEFORE this exact
/// `open` call executes: a worktree is repository-controlled content, so
/// anything with write access to the checkout (an agent's own earlier
/// approved write, or a background process) can swap the leaf
/// `computer-audit.jsonl` for a symlink to an arbitrary path in that
/// instant, and a plain `open` would silently follow it — this weft
/// PROCESS, not the sandboxed agent, would then be the one creating/
/// appending to a file outside the worktree.
///
/// `#[cfg(unix)]` adds `O_NOFOLLOW`: when the FINAL path component is a
/// symlink, the kernel's own `open(2)` fails outright (`ELOOP`) instead of
/// following it — this closes the TOCTOU window at the exact leaf
/// component, atomically, with no separate stat call of our own to race
/// against. It complements (does not replace) `refuse_symlinks`'s own
/// per-component walk, which still guards every PARENT directory
/// (`.weft` itself) — `O_NOFOLLOW` only ever inspects the LAST component of
/// the path being opened, never the ones leading up to it.
///
/// Windows keeps the pre-existing `refuse_symlinks`-only behavior for now —
/// a real per-component atomic open (an `openat`-style walk with
/// `FILE_FLAG_OPEN_REPARSE_POINT` at each step) is a follow-up, not required
/// to close THIS specific leaf race, which is what this flag targets.
///
/// `#[cfg(unix)]` also sets the
/// create mode to `0o600` (owner read/write only). Before this, a fresh
/// `computer-audit.jsonl` was created with whatever `open(2)`'s own default
/// (`0o666`) survives the process umask — `0o644` under the common `022`
/// umask — leaving desktop-activity metadata (target window queries, action
/// arguments, timestamps, outcomes) world/group-readable on a shared
/// machine. The screenshot files themselves were already `0o600` (see
/// `computer::screenshot_window`); this brings the audit log to the SAME
/// owner-only bar. Mode is only consulted by `open(2)` when it actually
/// CREATES a new file (`O_CREAT` with no existing inode) — which is why the
/// unix branch now ALSO validates the mode+owner of the file it actually
/// opened (`fstat` on the handle, so no path race) and reacts to a
/// permissive or foreign-owned pre-existing file exactly like the Windows
/// branch does for a permissive ACL: set it aside, re-create owner-only,
/// re-validate once, fail closed otherwise. A mere in-place `chmod` would
/// NOT do: another account's already-open read fd survives a chmod and
/// keeps reading every appended line, so the file must stop receiving
/// appends entirely.
async fn open_audit_file_for_append(path: &std::path::Path) -> std::io::Result<tokio::fs::File> {
    // On Windows the owner-only DACL rides the CREATION call itself
    // (`create_file_owner_only`) — a freshly created audit file (the first
    // append, or the new live file after each rotation) never exists for
    // even an instant under a permissive `WEFT_HOME` inherited ACL that
    // would expose window titles/actions/coordinates/outcomes to other
    // local accounts, and there is no post-create stamp another account's
    // pre-held handle could survive. But `OPEN_ALWAYS` only stamps that
    // DACL when it actually CREATES — a file already sitting at this path
    // (one created before this primitive existed, or by anything else)
    // keeps whatever ACL it has, so every open VALIDATES the live DACL of
    // the handle it got (`file_dacl_is_owner_only`): a file this primitive
    // created passes trivially; a permissive pre-existing one is set aside
    // to a `.insecure` sibling (its bytes were already exposed — preserved
    // for inspection, never appended to again) and the path is re-created
    // owner-only, re-validated once. Fail-CLOSED at every step: an
    // unverifiable DACL, a failed set-aside (the permissive file still
    // occupies the path), or a still-not-owner-only recreation (something
    // raced a new file in) all leave this line unlogged (the caller's
    // best-effort contract) rather than writing through a permissive ACL.
    #[cfg(windows)]
    {
        use crate::bus::inject::{
            create_file_owner_only, file_dacl_is_owner_only, set_aside_insecure, OwnerOnlyCreate,
        };
        let deny = || {
            Err(std::io::Error::other(
                "audit file owner-only open failed — refusing to write through a permissive ACL",
            ))
        };
        let Some(file) = create_file_owner_only(path, OwnerOnlyCreate::AppendOrCreate) else {
            return deny();
        };
        let verdict = file_dacl_is_owner_only(&file);
        if verdict == Some(true) {
            return Ok(tokio::fs::File::from_std(file));
        }
        // `None` (couldn't judge) fails closed WITHOUT setting the file
        // aside — rotating on an unproven verdict could churn a perfectly
        // good audit file to the sidecar on every append.
        if verdict.is_none() {
            return deny();
        }
        // Proven permissive: close our handle first (the move needs the
        // path free of this process's own open handle), set the file
        // aside, re-create owner-only, and re-validate the result once.
        drop(file);
        if !set_aside_insecure(path) {
            return deny();
        }
        let Some(file) = create_file_owner_only(path, OwnerOnlyCreate::AppendOrCreate) else {
            return deny();
        };
        if file_dacl_is_owner_only(&file) == Some(true) {
            return Ok(tokio::fs::File::from_std(file));
        }
        return deny();
    }
    #[cfg(not(windows))]
    {
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(path).await?;
        // The unix mirror of the Windows DACL validation above: `mode` only
        // applies when `open(2)` CREATES, so a pre-existing group/world-
        // readable file (or one another account owns) would keep receiving
        // appends. Validate the OPENED handle (fstat — immune to path
        // swaps), and on a violation set the file aside and re-create
        // owner-only, re-validated once; any failure refuses to log this
        // line rather than write through a permissive mode.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            use std::os::unix::fs::PermissionsExt as _;
            let deny = || {
                Err(std::io::Error::other(
                    "audit file owner-only open failed — refusing to write through a permissive \
                     mode",
                ))
            };
            // SAFETY: geteuid has no failure mode and touches no memory.
            let my_uid = unsafe { libc::geteuid() };
            let owner_only =
                |m: &std::fs::Metadata| m.permissions().mode() & 0o077 == 0 && m.uid() == my_uid;
            let Ok(meta) = file.metadata().await else {
                return deny();
            };
            if owner_only(&meta) {
                return Ok(file);
            }
            // Close our handle, set the permissive file aside (bytes were
            // already exposed under the old mode — preserved for inspection,
            // never appended to again), and take the path fresh.
            // `create_new` (O_EXCL): if something raced yet another file in,
            // fail closed this round instead of looping.
            drop(file);
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                return deny();
            };
            let aside = path.with_file_name(format!("{name}.insecure"));
            if tokio::fs::rename(path, &aside).await.is_err() {
                return deny();
            }
            let mut fresh_options = tokio::fs::OpenOptions::new();
            fresh_options
                .create_new(true)
                .append(true)
                .custom_flags(libc::O_NOFOLLOW)
                .mode(0o600);
            let fresh = fresh_options.open(path).await?;
            let Ok(meta) = fresh.metadata().await else {
                return deny();
            };
            if owner_only(&meta) {
                return Ok(fresh);
            }
            return deny();
        }
        #[cfg(not(unix))]
        {
            Ok(file)
        }
    }
}

/// The session's own Weft-managed output root for `(thread, dir[, wt])`,
/// before any output-specific suffix is appended — the shared base for
/// [`screenshot_out_dir`] (which appends a DIFFERENT suffix per lane; see
/// its own doc comment) and [`audit_log_path`] (which appends a per-lane
/// suffix too).
///
///  - worker lane (`dir` a direction id): `<weft_home>/computer/<thread>/
///    <dir>/wt-<id>`, `id` being the RESOLVED worktree id (see `wt`'s own doc
///    below for exactly which one). This lives OFF the worktree entirely, in
///    Weft's own managed
///    storage — see [`crate::paths::computer_output_root`]'s doc for why: in
///    a LINKED worktree, `git rev-parse --git-path info/exclude` resolves to
///    the CANONICAL repo's own SHARED `.git/info/exclude` (worktrees share
///    one gitdir's `info/` directory), so the OLD `<worktree>/.weft/...`
///    location — once "excluded" via `git::git_exclude(".weft/")` — silently
///    wrote a `.weft/` entry into the user's REAL repo metadata on the very
///    first screenshot/audit line: a hard violation of "never wire Weft's
///    own bookkeeping into a canonical repo". Nothing computer-use writes for
///    the worker lane touches the worktree at all anymore, so that
///    `git_exclude` call is gone too (see [`screenshot_out_dir`]/
///    [`audit_log_path`]) — there is no longer anything to exclude FROM. The
///    `wt-<id>` suffix keeps the SAME per-worktree isolation the old
///    worktree-rooted path got for free: two worktrees of the
///    same multi-repo direction still never share an output directory,
///    whether `id` came from an EXPLICIT pin or the first-worktree fallback
///    below.
///  - lead lane (`dir == bus::LEAD`): `<weft_home>/computer/<thread>/lead` —
///    NOT
///    the lead's own scratch cwd (`<weft_home>/leads/<thread>`), but that
///    directory is a real working dir the LEAD AGENT ITSELF writes into: an
///    agent (or any background process with the same access) could swap the
///    `.weft`/`screenshots` PARENT directories for symlinks in the window
///    between [`refuse_symlinks`]'s component walk and the later
///    `create_dir_all`/open — the writers only apply `O_NOFOLLOW` to the
///    LEAF, so a swapped parent would be silently followed and Weft (with its
///    own privileges, not the sandboxed agent's) would write outside the
///    session directory. Rather than re-plumbing every writer onto openat-
///    style verified directory handles, the lead's output moves under the
///    SAME Weft-managed root the worker lane already uses — a tree Weft
///    creates for itself and never hands to any agent as a working
///    directory — which removes the agent-writable-parent premise of the
///    race by construction (the same reasoning that moved
///    the WORKER lane off the worktree). `refuse_symlinks` stays as defense
///    in depth on both lanes. Old audit/screenshot files left in the scratch
///    cwd from before this change are simply orphaned (best-effort logs; never
///    read back).
///
/// `wt`: the CALLER's own worktree id, when it
/// could resolve one — see `inject::computer_url`'s doc for who sets this
/// and why (a multi-repo direction has MORE THAN ONE worktree row, and
/// without this every worker sharing that direction fell back to whichever
/// one happened to be first — screenshots/audit for a worker in repo B could
/// silently land in repo A's namespace instead). CLOSED-SET validated here:
/// `Some(id)` is only honored when `id` names a worktree that actually
/// belongs to THIS direction (which is itself already confirmed to belong to
/// THIS thread, below) — a forged/foreign worktree id can never redirect
/// output into another direction's (or another thread's) namespace.
///
/// an EXPLICIT `wt` that fails that check is now
/// FAIL-CLOSED — `None` — rather than silently falling back to "the first
/// worktree of this direction" as it used to. That old fallback was fine for
/// an ABSENT `wt` (there was never a specific worktree pinned to begin with),
/// but for an EXPLICIT-but-invalid one (deleted, or naming a worktree of a
/// DIFFERENT direction/repo) it actively misdirected output: in a multi-repo
/// direction, a worker session that explicitly pinned worktree B and lost the
/// race against a deletion (or was handed a forged/stale id) would have had
/// its screenshots and audit log silently written into worktree A's
/// namespace instead — exposing that OTHER repo's on-screen pixels and
/// activity to the wrong worker. Only a genuinely ABSENT `wt` (`None`) still
/// falls back to "first worktree for this direction"; an explicit id that
/// doesn't resolve now fails the whole call closed instead of picking a
/// different worktree on its own. [`WtParam`]
/// extends the SAME fail-closed rule one parse step earlier, for a `?wt=`
/// that isn't even numeric — that rejection happens in `handle_computer`,
/// before this function (or any of its callers) is ever reached.
///
/// `None` on any failure (DB error, unresolvable path, no worktree at all
/// for this direction, a numeric `dir` that doesn't resolve to a direction
/// belonging to THIS thread, or — per the explicit-pin rule above — an EXPLICIT `wt`
/// that doesn't name a worktree of this direction) — callers turn that into
/// their own soft-failure text rather than a 500.
async fn session_root(db: &Db, thread: i32, dir: &str, wt: Option<i32>) -> Option<std::path::PathBuf> {
    if dir == crate::bus::LEAD {
        // Weft-managed, never agent-writable — see
        // this function's own lead-lane doc above.
        let root = crate::paths::computer_output_root()
            .ok()?
            .join(thread.to_string())
            .join(crate::bus::LEAD);
        return Some(root);
    }
    let direction_id = dir.parse::<i32>().ok()?;
    match repo::get_direction(db, direction_id).await {
        Ok(Some(d)) if d.thread_id == thread => {}
        _ => return None,
    }
    let worktrees = repo::list_worktrees(db, Some(direction_id)).await.ok()?;
    // An EXPLICIT pin must hit an actual worktree of THIS
    // direction, or the whole call fails closed — silently falling back to
    // the first worktree would write this session's screenshots/audit into a
    // DIFFERENT repo's namespace in a multi-repo direction. An ABSENT `wt`
    // still falls back to the first worktree, same as always.
    let resolved_id = match wt {
        Some(id) => worktrees.iter().find(|w| w.id == id).map(|w| w.id),
        None => worktrees.first().map(|w| w.id),
    }?;
    let root = crate::paths::computer_output_root()
        .ok()?
        .join(thread.to_string())
        .join(dir)
        .join(format!("wt-{resolved_id}"));
    Some(root)
}

/// remove a thread's ENTIRE
/// computer-use output subtree — `<weft_home>/computer/<thread>/`, which holds
/// every lead/worker session's screenshots and rotated audit logs (see
/// [`session_root`] for the layout underneath it) — as part of deleting the
/// thread. Without this, the persistent per-thread tree these actions write
/// outlives its thread indefinitely: the per-session retention limits only
/// bound a LIVE session's own growth, and a deleted thread's sessions never
/// run again to enforce anything, so repeatedly creating, using, and deleting
/// threads would grow this directory without bound.
///
/// Best-effort: a failed/absent delete must never block the thread cascade
/// (mirrors `materialize::cleanup_worktrees`'s own best-effort tail). Bounded
/// to this thread's own subtree only — the single `join` is on `thread`, a
/// plain integer with no path-separator/`..` component, so there is nothing to
/// escape [`crate::paths::computer_output_root`] with.
pub(crate) fn remove_computer_output_for_thread(thread: i32) {
    // walk every
    // component from `weft_home` down WITHOUT following symlinks before the
    // recursive delete — `computer_output_root`'s own `create_dir_all` happily
    // follows a pre-planted symlink at `computer/`, and `remove_dir_all`
    // through one would delete `<target>/<thread>` OUTSIDE Weft-managed
    // storage. Same anchored walk as the write path's
    // [`refuse_symlinked_output`]; a refused chain just skips the best-effort
    // cleanup (never gates the delete).
    let Ok(home) = crate::paths::weft_home() else {
        return;
    };
    let Ok(dir) = refuse_symlinks(&home, &["computer", &thread.to_string()]) else {
        return;
    };
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// remove ONE worker direction's
/// computer-output subtree — `<weft_home>/computer/<thread>/<direction_id>/`,
/// holding that direction's per-worktree screenshots and rotated audit logs (see
/// [`session_root`]'s worker layout). `commands::delete_repo` removes a repo's
/// directions while the parent thread SURVIVES, so [`remove_computer_output_for_
/// thread`] (whole-thread) would wrongly wipe the thread's surviving directions;
/// this prunes only the deleted direction's tree, which `session_root` could
/// never reach again once the direction row is gone.
///
/// Bounded: `dir` must parse as a plain direction id (a bare integer, no
/// separator / `..`), matching [`session_root`]'s own `dir.parse::<i32>()` — any
/// other shape (a lead lane, a malformed token) is IGNORED rather than joined,
/// so there is nothing to escape [`crate::paths::computer_output_root`] with.
/// Best-effort, never gates the delete.
pub(crate) fn remove_computer_output_for_direction(thread: i32, dir: &str) {
    if dir.parse::<i32>().is_err() {
        return;
    }
    // same no-follow
    // ancestor walk as [`remove_computer_output_for_thread`] — a symlinked
    // `computer/` or `<thread>/` ancestor must never be traversed into a
    // recursive delete outside Weft-managed storage.
    let Ok(home) = crate::paths::weft_home() else {
        return;
    };
    let Ok(path) = refuse_symlinks(&home, &["computer", &thread.to_string(), dir]) else {
        return;
    };
    if path.exists() {
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// remove ONE session-only
/// worker's output subtree — `<weft_home>/computer/<thread>/<dir>/wt-<wt>/`
/// (see [`session_root`]'s worker layout) — for `commands::delete_repo`
/// removing a worktree whose direction is OWNED BY ANOTHER repo and survives.
/// [`remove_computer_output_for_direction`] would wrongly sweep the surviving
/// direction's OTHER worktrees; this prunes only the deleted worktree's own
/// namespace, which [`session_root`] can never resolve again once the worktree
/// row is gone. Same bounds and no-follow ancestor walk as the other removers
/// : `dir` must be a plain direction id, `wt` is a plain integer,
/// and a refused/symlinked chain just skips the best-effort cleanup.
pub(crate) fn remove_computer_output_for_worktree(thread: i32, dir: &str, wt: i32) {
    if dir.parse::<i32>().is_err() {
        return;
    }
    let Ok(home) = crate::paths::weft_home() else {
        return;
    };
    let leaf = format!("wt-{wt}");
    let Ok(path) = refuse_symlinks(&home, &["computer", &thread.to_string(), dir, &leaf]) else {
        return;
    };
    if path.exists() {
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// undo THIS call's own
/// screenshot write after the post-capture revocation recheck found the route
/// revoked — the capture closure's `create_dir_all(out_dir)` + PNG save can
/// RECREATE a session subtree a concurrent delete already removed (the
/// closure's pre-capture revocation check is check-then-act across the whole
/// blocking capture). Removes the screenshots subtree the save (re)created,
/// then prunes now-empty ancestor directories deepest-first, stopping at the
/// shared `<weft_home>/computer` root (which outlives any one session) or at
/// the first non-empty directory — the pruning uses non-recursive
/// `std::fs::remove_dir`, which fails on a non-empty directory and ends the
/// walk, so a surviving sibling session's tree can never be swept.
///
/// Symlink containment: the path is re-walked from
/// `weft_home` with the same no-follow [`refuse_symlinks`] verification the
/// delete-side removers use, re-checked HERE (not trusted from resolution
/// time) because a component could have been swapped for a symlink since —
/// any refused chain, a path outside `<weft_home>/computer`, or any
/// non-normal component (`..`, a root) just skips this best-effort cleanup.
fn remove_recreated_screenshot_output(out_dir: &std::path::Path) {
    let Ok(home) = crate::paths::weft_home() else {
        return;
    };
    let Ok(rel) = out_dir.strip_prefix(&home) else {
        return;
    };
    let mut parts: Vec<&str> = Vec::new();
    for comp in rel.components() {
        match comp {
            std::path::Component::Normal(c) => match c.to_str() {
                Some(s) => parts.push(s),
                None => return,
            },
            _ => return,
        }
    }
    if parts.first() != Some(&"computer") || parts.len() < 2 {
        return;
    }
    let Ok(checked) = refuse_symlinks(&home, &parts) else {
        return;
    };
    if checked.exists() {
        let _ = std::fs::remove_dir_all(&checked);
    }
    let Ok(stop) = refuse_symlinks(&home, &["computer"]) else {
        return;
    };
    let mut cur = checked.parent().map(std::path::Path::to_path_buf);
    while let Some(p) = cur {
        if p == stop || !p.starts_with(&stop) {
            break;
        }
        if std::fs::remove_dir(&p).is_err() {
            break;
        }
        cur = p.parent().map(std::path::Path::to_path_buf);
    }
}

/// does `(thread, dir, wt)`
/// STILL denote a live session? The per-session bearer stays valid for the
/// whole process lifetime and each Axum request is independent of the engine,
/// so a thread/direction deleted AFTER the token was minted — `delete_thread_
/// cascade` purges asks/grants and stops engines, but can neither revoke an
/// already-minted token nor abort an in-flight HTTP call — must still be
/// refused before any OS work, or a stale caller could keep driving (or newly
/// submit calls against) the human's desktop under a deleted identity.
///
/// Mirrors [`session_root`]'s OWN DB validation: a worker lane's `dir` must
/// resolve to a direction of THIS exact thread and, if a worktree is pinned,
/// that worktree must belong to the direction; the lead lane requires only that
/// the thread row still exists. Fail-CLOSED — any DB error resolves to `false`
/// (not live), so a transient store hiccup can never fail OPEN into driving a
/// desktop whose owning session may already be gone.
/// The one rendering of the "session deleted" refusal,
/// used by the [`recheck_after_guard`] post-queue revalidation.
const SESSION_GONE_MSG: &str =
    "this computer-use session no longer exists (its issue or direction was deleted) — refused";

/// Per-thread computer-use route revocation state. A whole-
/// thread delete revokes EVERY route; a repo delete revokes only the specific
/// worker directions it removes, leaving the thread's surviving directions (and
/// its lead lane) live — so the state has to distinguish the two, or a repo
/// delete would permanently strand a multi-repo thread's other directions.
///
/// Why a revocation MAP rather than a blanket DB-liveness check on every request:
/// the per-session bearer is a process-lifetime HMAC the delete path can't
/// rescind, and the Axum request is independent of the engine it stops, so a
/// token minted before a delete stays cryptographically valid. Recording the
/// deletion here revokes those tokens WITHOUT a DB round-trip on the common
/// live-session request — and without coupling every request (or the many
/// synthetic-identity tests, which deliberately never set up matching DB rows)
/// to a DB shape. Only a genuinely-deleted route is ever in this map; such a
/// request then pays ONE [`session_is_live`] check to tell a real deletion
/// (refuse) from a REUSED id (allow), so id reuse can never strand a fresh
/// session.
#[derive(Clone)]
enum RouteRevocation {
    /// Whole thread gone (`delete_thread` / `delete_workspace`): every direction
    /// and the lead lane are revoked.
    Whole,
    /// Only parts of the thread gone (`delete_repo`, which leaves the thread
    /// itself alive):
    ///  - `dirs` — worker directions the deleted repo OWNED, keyed by the route
    ///    `dir` string (`direction_id.to_string()`); every lane/worktree of such
    ///    a direction is revoked.
    ///  - `worktrees`:
    ///    `(dir, wt)` pairs for SESSION-ONLY workers — a worktree/session the
    ///    deleted repo contributed to a direction OWNED BY ANOTHER repo. The
    ///    direction (and its other repos' sibling worktrees) survive, so a
    ///    dir-level revocation would be wrong — but the deleted worktree's own
    ///    bearer is bound to exactly `(thread, dir, wt)` and must stop working,
    ///    or a surviving child holding it could keep driving the desktop
    ///    (silently, under a standing Full/Always grant) after its session rows
    ///    and worktree are gone.
    Partial {
        dirs: std::collections::HashSet<String>,
        worktrees: std::collections::HashSet<(String, i32)>,
    },
}

fn revoked_computer_routes(
) -> &'static std::sync::Mutex<std::collections::HashMap<i32, RouteRevocation>> {
    static MAP: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<i32, RouteRevocation>>,
    > = std::sync::OnceLock::new();
    MAP.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Revoke EVERY route for `thread` — `commands::delete_thread` /
/// `delete_workspace_after_fence`. Supersedes any prior direction-level entry.
pub(crate) fn revoke_computer_routes(thread: i32) {
    revoked_computer_routes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(thread, RouteRevocation::Whole);
}

/// Revoke ONE worker direction of `thread` — `commands::delete_repo`, which
/// removes a repo's directions while the thread itself survives. Adds to (never
/// downgrades) the thread's entry: a pre-existing `Whole` stays `Whole`.
pub(crate) fn revoke_computer_route_dir(thread: i32, dir: String) {
    let mut map = revoked_computer_routes()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match map.get_mut(&thread) {
        Some(RouteRevocation::Whole) => {}
        Some(RouteRevocation::Partial { dirs, .. }) => {
            dirs.insert(dir);
        }
        None => {
            let mut dirs = std::collections::HashSet::new();
            dirs.insert(dir);
            map.insert(
                thread,
                RouteRevocation::Partial { dirs, worktrees: std::collections::HashSet::new() },
            );
        }
    }
}

/// Revoke ONE session-only worker `(thread, dir, wt)`
/// : `commands::delete_repo` removing a worktree whose
/// direction is OWNED BY ANOTHER repo and survives. Adds to (never downgrades)
/// the thread's entry, exactly like [`revoke_computer_route_dir`]. Recording
/// the entry also flips [`computer_routes_revoked`] for the thread, so the
/// async entry gate starts paying the [`session_is_live`] DB check — which
/// fails closed for the deleted worktree row while the direction's surviving
/// sibling worktrees keep passing it.
pub(crate) fn revoke_computer_route_session(thread: i32, dir: String, wt: i32) {
    let mut map = revoked_computer_routes()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match map.get_mut(&thread) {
        Some(RouteRevocation::Whole) => {}
        Some(RouteRevocation::Partial { worktrees, .. }) => {
            worktrees.insert((dir, wt));
        }
        None => {
            let mut worktrees = std::collections::HashSet::new();
            worktrees.insert((dir, wt));
            map.insert(
                thread,
                RouteRevocation::Partial { dirs: std::collections::HashSet::new(), worktrees },
            );
        }
    }
}

/// Prior revocation state for a set of threads, captured BEFORE a delete revokes
/// them so a FAILED cascade can restore EXACTLY what was there.
/// A blanket un-revoke would drop a
/// revocation that an EARLIER successful delete already published for the same
/// thread, re-opening that stale route's bearer.
pub(crate) struct RevocationSnapshot(Vec<(i32, Option<RouteRevocation>)>);

/// Serializes every delete flow's revocation TRANSACTION — snapshot → publish →
/// cascade → restore-or-commit.
/// Without it, two overlapping deletes touching routes of the SAME thread can
/// both snapshot the prior map before either publishes; if one cascade succeeds
/// and the other later fails, the failing operation's restore replaces the
/// whole thread entry with its STALE snapshot, erasing the successful delete's
/// revocation — whose route's still-valid bearer then resumes desktop calls
/// (the request gate only runs its liveness query while a revocation entry
/// exists). Deletes are rare, human-initiated operations already serialized on
/// heavier locks (the engine-admission write fence), so one more coarse
/// transaction lock is the simplest correct shape — held by `commands`'
/// delete flows from before their [`snapshot_revocations`] until their
/// restore-or-commit point.
pub(crate) fn revocation_txn_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub(crate) fn snapshot_revocations(threads: &[i32]) -> RevocationSnapshot {
    let map = revoked_computer_routes()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    RevocationSnapshot(threads.iter().map(|&t| (t, map.get(&t).cloned())).collect())
}

pub(crate) fn restore_revocations(snapshot: RevocationSnapshot) {
    let mut map = revoked_computer_routes()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (thread, prior) in snapshot.0 {
        match prior {
            Some(state) => {
                map.insert(thread, state);
            }
            None => {
                map.remove(&thread);
            }
        }
    }
}

/// Coarse: does `thread` have ANY revoked route? The async entry gate and
/// [`recheck_after_guard`] use this to decide whether to run the direction-
/// precise [`session_is_live`] DB check — a never-deleted thread skips it.
fn computer_routes_revoked(thread: i32) -> bool {
    revoked_computer_routes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&thread)
}

/// Sync + route-precise: is THIS exact `(thread, dir, wt)` route revoked? Used
/// inside a blocking-pool closure (the screenshot capture) where an async
/// [`session_is_live`] can't run but a coarse thread-level refuse would be wrong
/// — `delete_repo` leaves the thread's sibling directions live. `wt` is part
/// of the key: a SESSION-ONLY worker deletion
/// revokes one exact `(dir, wt)` while the direction's surviving sibling
/// worktrees (and its `wt`-less lead-style resolution) stay live, so the check
/// has to be worktree-precise where a worktree identity exists at all.
fn route_revoked_sync(thread: i32, dir: &str, wt: Option<i32>) -> bool {
    match revoked_computer_routes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&thread)
    {
        Some(RouteRevocation::Whole) => true,
        Some(RouteRevocation::Partial { dirs, worktrees }) => {
            dirs.contains(dir)
                || wt.is_some_and(|w| worktrees.iter().any(|(d, rw)| d == dir && *rw == w))
        }
        None => false,
    }
}

async fn session_is_live(db: &Db, thread: i32, dir: &str, wt: Option<i32>) -> bool {
    if dir == crate::bus::LEAD {
        return matches!(repo::get_thread(db, thread).await, Ok(Some(_)));
    }
    let Ok(direction_id) = dir.parse::<i32>() else {
        return false;
    };
    match repo::get_direction(db, direction_id).await {
        Ok(Some(d)) if d.thread_id == thread => {}
        _ => return false,
    }
    let Some(wt_id) = wt else {
        return true;
    };
    matches!(
        repo::list_worktrees(db, Some(direction_id)).await,
        Ok(wts) if wts.iter().any(|w| w.id == wt_id)
    )
}

/// Build `base/components[0]/components[1]/...`, refusing if ANY existing
/// path component along the way is a symlink.
/// Originally guarded a worktree's own `.weft` subtree (repository-controlled
/// content a sandboxed agent's own approved writes could tamper with);
/// the worker lane's own `base` lives off the worktree
/// entirely, in a Weft-owned directory under `weft_home` (see
/// [`session_root`]'s own doc) — that directory is created by Weft itself,
/// never handed to a sandboxed agent to write into directly, so the
/// worktree-tampering scenario this originally guarded against is largely
/// closed by construction now. Kept anyway as defense in depth (a SEPARATE,
/// same-uid process on the human's own machine could still reach into
/// `weft_home` — a residual risk noted here, not fixed by this change) and
/// because the lead lane's `base` (`<weft_home>/leads/<thread>`) is still a
/// real, git-init'd working directory other tooling can touch.
///
/// Checked component-by-component from `base` via `symlink_metadata` (never
/// plain `metadata`, so a symlink is caught even when it points at something
/// real rather than dangling): a component that doesn't exist yet is fine —
/// it hasn't been substituted for anything, and the caller's own
/// `create_dir_all`/`OpenOptions::create` will make it fresh — while one that
/// exists and IS a symlink is refused outright, named in the error so the
/// audit/screenshot caller can report which exact segment was compromised.
/// Mirrors `skills::inject::builtin_write_path_is_safe`'s own
/// component-walk shape (a different module, same hazard).
fn refuse_symlinks(base: &std::path::Path, components: &[&str]) -> Result<std::path::PathBuf, String> {
    let mut current = base.to_path_buf();
    for seg in components {
        current.push(seg);
        if let Ok(meta) = std::fs::symlink_metadata(&current) {
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "refusing to write through a symlink at {}",
                    current.display()
                ));
            }
        }
    }
    Ok(current)
}

/// Resolve `<root>/<leaf>` for a computer-use output write (the screenshots
/// directory or the audit log), refusing if ANY path component from
/// `weft_home` down — `computer/`, `<thread>/`, `<dir>/`, `wt-<id>/`, AND the
/// final `<leaf>` — is a symlink.
///
/// [`refuse_symlinks`] alone only walks the components appended AFTER its
/// `base`; both output resolvers passed the already-joined [`session_root`] as
/// that `base` with just the leaf as a component, so a symlink pre-created at
/// an ANCESTOR — `computer/` itself, or the per-thread / per-direction /
/// per-worktree directories [`session_root`] resolves, every one of which
/// [`crate::paths::computer_output_root`]'s own `create_dir_all` would happily
/// FOLLOW — escaped the check entirely, while `O_NOFOLLOW` (see
/// [`append_audit`]) guarded only the final audit filename. A same-uid process
/// could therefore redirect screenshots and audit appends outside
/// Weft-managed storage, including into a pre-created attacker-readable file.
///
/// Anchoring the walk at `weft_home` — the one directory Weft owns outright, a
/// symlink THERE being the user's own doing (the same-uid isolation ceiling
/// this module's top-of-file doc already accepts) — and re-deriving every
/// intermediate segment from the resolved `root` closes that: a compromised
/// ancestor is now named and refused BEFORE either output is created or
/// opened. `root` is always a `session_root` result (built by joining onto
/// `computer_output_root` = `weft_home/computer`), so the strip below cannot
/// fail in production; a `root` that somehow does not live under `weft_home`
/// fails CLOSED rather than silently skipping the ancestor walk.
fn refuse_symlinked_output(
    root: &std::path::Path,
    leaf: &str,
) -> Result<std::path::PathBuf, String> {
    let home = crate::paths::weft_home().map_err(|e| format!("weft home unavailable: {e}"))?;
    let rel = root.strip_prefix(&home).map_err(|_| {
        format!(
            "refusing to write computer output outside weft home: {}",
            root.display()
        )
    })?;
    let mut components: Vec<&str> = Vec::new();
    for comp in rel.components() {
        match comp {
            std::path::Component::Normal(seg) => match seg.to_str() {
                Some(s) => components.push(s),
                None => {
                    return Err(format!(
                        "refusing to write through a non-UTF8 path component under {}",
                        home.display()
                    ))
                }
            },
            // A session_root is only ever `weft_home` + normal names; anything
            // else (a stray `..`, a rooted segment) is not a shape we resolve,
            // so refuse rather than walk it.
            _ => {
                return Err(format!(
                    "refusing to write through a non-normal path component in {}",
                    root.display()
                ))
            }
        }
    }
    components.push(leaf);
    refuse_symlinks(&home, &components)
}

/// Resolve the screenshot output directory for `(thread, dir[, wt])`:
///  - worker lane: `<session_root>/screenshots` — a dedicated, Weft-managed
///    directory (see [`session_root`]'s own doc for the move
///    off the worktree). No `git::git_exclude` call anymore: this directory
///    was never inside a git-tracked worktree to begin with, so there is
///    nothing left to exclude FROM (and nothing left that could leak a
///    `.weft/` entry into a canonical repo's `info/exclude`).
///  - lead lane: `<session_root>/screenshots` too
///    first gave leads a dedicated subdirectory
///    (so retention pruning never operates over unrelated files), and
///    the lead's WHOLE
///    session root moved under Weft-managed storage (see [`session_root`]'s
///    lead-lane doc for the agent-writable-parent race that closed) — at
///    which point the lead lane needs no special `.weft/` layer anymore and
///    both lanes share ONE shape.
///
/// `Err` (not silently `None`) on a resolution failure OR a refused symlink
///  — callers surface the
/// SPECIFIC reason (missing worktree vs. a compromised output path) to the
/// calling agent rather than one flattened "no worktree" text for both.
async fn screenshot_out_dir(db: &Db, thread: i32, dir: &str, wt: Option<i32>) -> Result<std::path::PathBuf, String> {
    let root = session_root(db, thread, dir, wt)
        .await
        .ok_or_else(|| "no worktree for this session".to_string())?;
    refuse_symlinked_output(&root, "screenshots")
}

/// Resolve the audit log path for `(thread, dir[, wt])`:
///  - worker lane: `<session_root>/computer-audit.jsonl` — directly under the
///    dedicated per-(thread, dir, wt) directory [`session_root`] resolves
///  ; no `.weft/` layer needed since the whole
///    directory is already private to this one session, and no
///    `git::git_exclude` call anymore for the SAME reason [`screenshot_out_dir`]
///    no longer needs one.
///  - lead lane: `<session_root>/computer-audit.jsonl` too —
///    the lead's session root lives
///    under Weft-managed storage (see [`session_root`]'s lead-lane doc), so
///    the `.weft/` layer its scratch-cwd audit log used to hide behind is no
///    longer needed: the whole directory is private to this one session, same
///    as the worker lane, and both lanes share ONE shape.
///
/// `None` (best-effort, per [`append_audit`]'s own doc) on a resolution
/// failure OR a refused symlink (via
/// [`refuse_symlinks`]) — a compromised output path just means this one call
/// goes unlogged, same as any other audit-write failure.
async fn audit_log_path(db: &Db, thread: i32, dir: &str, wt: Option<i32>) -> Option<std::path::PathBuf> {
    let root = session_root(db, thread, dir, wt).await?;
    refuse_symlinked_output(&root, "computer-audit.jsonl").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // —— always-inject + server-side disabled gate ——

    /// The static tool description must itself say this needs enabling in
    /// Settings — makes injection unconditional
    /// (`weft_computer` is now handed to every issue-lead/worker engine
    /// regardless of the setting), an agent must not have to guess why every
    /// call comes back `disabled` the first time it tries.
    #[test]
    fn tool_description_says_it_needs_enabling_in_settings() {
        let specs = computer_tool_specs();
        let desc = specs[0]["description"].as_str().unwrap_or_default();
        assert!(
            desc.to_lowercase().contains("settings"),
            "the tool description must mention enabling this in Settings: {desc}"
        );
    }

    // —— `?wt=` three-state parsing ——

    #[test]
    fn wt_param_parse_distinguishes_absent_explicit_and_invalid() {
        let mut q = HashMap::new();
        assert!(matches!(WtParam::parse(&q), WtParam::Absent), "no wt key at all");

        q.insert("wt".to_string(), "42".to_string());
        assert!(matches!(WtParam::parse(&q), WtParam::Explicit(42)));

        q.insert("wt".to_string(), "abc".to_string());
        assert!(matches!(WtParam::parse(&q), WtParam::Invalid), "non-numeric");

        q.insert("wt".to_string(), String::new());
        assert!(matches!(WtParam::parse(&q), WtParam::Invalid), "present but empty");

        q.insert("wt".to_string(), "-7".to_string());
        assert!(
            matches!(WtParam::parse(&q), WtParam::Explicit(-7)),
            "a negative number still parses as i32 — session_root's own closed-set check is what \
             ultimately rejects an id that doesn't name a real worktree, not this parse step"
        );
    }

    #[test]
    fn wt_param_resolve_only_invalid_rejects_absent_and_explicit_pass_through() {
        assert_eq!(WtParam::Absent.resolve(), Ok(None));
        assert_eq!(WtParam::Explicit(5).resolve(), Ok(Some(5)));
        assert!(WtParam::Invalid.resolve().is_err());
    }

    /// The bearer binds the EXACT worktree, not just `(thread, dir)`. A token
    /// minted for one `wt` must NOT verify for a sibling `wt` (the hijack), for
    /// the absent case, or for a different `(thread, dir)`; the absent token is
    /// symmetric — it must not verify for any explicit `wt`.
    #[test]
    fn computer_token_binds_to_the_worktree_not_just_thread_dir() {
        let t7 = computer_session_token(1, "10", Some(7));
        assert!(verify_computer_token(1, "10", Some(7), &t7), "the exact wt must verify");
        assert!(
            !verify_computer_token(1, "10", Some(8), &t7),
            "a SIBLING worktree id must not verify a token minted for a different one"
        );
        assert!(
            !verify_computer_token(1, "10", None, &t7),
            "the absent-worktree case must not verify a token minted for an explicit wt"
        );
        assert!(!verify_computer_token(2, "10", Some(7), &t7), "still bound to thread");
        assert!(!verify_computer_token(1, "20", Some(7), &t7), "still bound to dir");

        let t_none = computer_session_token(1, "10", None);
        assert!(verify_computer_token(1, "10", None, &t_none), "the absent case must verify itself");
        assert!(
            !verify_computer_token(1, "10", Some(7), &t_none),
            "an explicit wt must not verify a token minted for the absent case"
        );
        // The `none` marker for the absent case can never collide with any
        // explicit id's `wt<id>` representation.
        assert_ne!(t7, t_none);
    }

    /// Rotating an identity's token generation (what every fresh injection
    /// does before minting) must invalidate the bearer minted before it —
    /// the replaced session's old child gets a bare 401 at the entry gate —
    /// while a sibling identity's own bearer stays untouched.
    #[test]
    fn rotating_the_session_token_generation_invalidates_the_prior_bearer() {
        let old = computer_session_token(933_001, "70", Some(1));
        assert!(verify_computer_token(933_001, "70", Some(1), &old), "current render verifies");
        let sibling = computer_session_token(933_001, "70", Some(2));

        let fresh = mint_committed(933_001, "70", Some(1));

        assert!(
            !verify_computer_token(933_001, "70", Some(1), &old),
            "the pre-rotation bearer must be refused once the identity was re-injected"
        );
        assert_ne!(old, fresh, "the replacement session's bearer is a different token");
        assert!(verify_computer_token(933_001, "70", Some(1), &fresh), "only the fresh one verifies");
        assert_eq!(
            fresh,
            computer_session_token(933_001, "70", Some(1)),
            "the atomic mint IS the current render — no separate re-read needed"
        );
        assert!(
            verify_computer_token(933_001, "70", Some(2), &sibling),
            "a sibling worktree's own bearer must survive the rotation"
        );
    }

    /// Mint and immediately accept the hand-off. These tests exercise token and
    /// generation semantics, not the ownership protocol, so they stand in for
    /// "a live child took this bearer". Without the commit the `MintGuard`
    /// drops at the end of the statement and revokes what was just minted —
    /// which is the guard working, and exactly what it is for.
    fn mint_committed(thread: i32, dir: &str, wt: Option<i32>) -> String {
        let minted = rotate_and_mint_computer_session_token(thread, dir, wt);
        let _ = minted.guard.commit();
        minted.token
    }

    /// Stopping a session must kill its bearer even though no replacement is
    /// ever minted: rotation happens at INJECTION, and a
    /// stopped-but-not-relaunched session never injects again, so without an
    /// explicit revoke the old token would verify forever. A sibling
    /// worktree's bearer — a different identity — must be untouched, and the
    /// next injection for the revoked identity must still produce a working
    /// token (resume works).
    #[test]
    fn revoking_on_stop_kills_the_bearer_without_minting_a_replacement() {
        let stopped = computer_session_token(933_201, "70", Some(1));
        let sibling = computer_session_token(933_201, "70", Some(2));
        assert!(verify_computer_token(933_201, "70", Some(1), &stopped));

        let live = session_token_generation(933_201, "70", Some(1));
        assert!(revoke_computer_session_token_generation(933_201, "70", Some(1), live));

        assert!(
            !verify_computer_token(933_201, "70", Some(1), &stopped),
            "a stopped session's bearer must stop verifying the instant Stop lands"
        );
        assert!(
            verify_computer_token(933_201, "70", Some(2), &sibling),
            "revoking one worktree's identity must not disturb its sibling's"
        );

        // Resume: the respawn path re-injects, which mints under the bumped
        // generation — so the resumed child gets a working bearer.
        let resumed = mint_committed(933_201, "70", Some(1));
        assert_ne!(resumed, stopped, "resume must not hand back the revoked token");
        assert!(
            verify_computer_token(933_201, "70", Some(1), &resumed),
            "a re-injected session's fresh bearer must verify"
        );
    }

    /// A revoke that lands between a caller's liveness snapshot and its own
    /// re-mint must not be undone by that mint. The dispatch path re-checks
    /// cancellation under the same lock before re-minting, and revokes again
    /// when the post-connect check rejects the client — this pins the token-
    /// level property both rely on: the LAST operation wins, so a revoke after
    /// a mint leaves nothing valid behind.
    #[test]
    fn a_revoke_after_a_mint_leaves_no_valid_bearer() {
        let minted = rotate_and_mint_computer_session_token(933_401, "70", Some(3));
        let generation = minted.guard.commit();
        assert!(verify_computer_token(933_401, "70", Some(3), &minted.token));

        // The doomed child's connection is torn down: revoke what we minted.
        assert!(revoke_computer_session_token_generation(
            933_401,
            "70",
            Some(3),
            generation
        ));

        assert!(
            !verify_computer_token(933_401, "70", Some(3), &minted.token),
            "a bearer minted for a child that is then torn down must not survive it"
        );
    }

    /// Revoking an identity that never had a token is harmless — the entry is
    /// created at the bumped generation and the first injection mints against
    /// it. Guards the `stop` path for engines constructed without computer use
    /// (concierge/curator leads, workers with an unresolved worktree), which
    /// call the revoke unconditionally.
    #[test]
    fn revoking_an_identity_that_never_minted_is_harmless() {
        assert!(
            !revoke_computer_session_token_generation(933_301, "lead", None, 1),
            "a generation this identity never minted must not be revocable"
        );
        let minted = mint_committed(933_301, "lead", None);
        assert!(verify_computer_token(933_301, "lead", None, &minted));
    }

    /// Bump and render share one critical section: each of two back-to-back
    /// rotations mints its OWN generation's token, so overlapping injections
    /// can never both end up holding the latest-generation bearer — at most
    /// the LAST rotation's token verifies.
    #[test]
    fn overlapping_rotations_never_share_the_latest_bearer() {
        let first = mint_committed(933_101, "70", None);
        let second = mint_committed(933_101, "70", None);
        assert_ne!(first, second, "each rotation mints its own generation's token");
        assert!(
            !verify_computer_token(933_101, "70", None, &first),
            "the earlier rotation's bearer must already be dead"
        );
        assert!(
            verify_computer_token(933_101, "70", None, &second),
            "only the last rotation's bearer verifies"
        );
    }

    #[test]
    fn audit_line_is_one_json_object_per_line() {
        let args = json!({"action": "left_click", "coordinate": [1, 2]});
        let entry = AuditEntry {
            ts_ms: 12345,
            thread: 1,
            dir: "lead",
            action: "left_click",
            window_query: "notes",
            window_id: Some(9),
            args: &args,
            outcome: "ok",
        };
        let line = audit_line(&entry).unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1, "exactly one line, not pretty-printed");
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["ts_ms"], 12345);
        assert_eq!(parsed["thread"], 1);
        assert_eq!(parsed["dir"], "lead");
        assert_eq!(parsed["action"], "left_click");
        assert_eq!(parsed["window_query"], "notes");
        assert_eq!(parsed["window_id"], 9);
        assert_eq!(parsed["outcome"], "ok");
        assert_eq!(parsed["args"]["action"], "left_click");
    }

    #[test]
    fn audit_line_serializes_a_missing_window_id_as_null() {
        let args = json!({"action": "wait"});
        let entry = AuditEntry {
            ts_ms: 1,
            thread: 1,
            dir: "lead",
            action: "wait",
            window_query: "",
            window_id: None,
            args: &args,
            outcome: "ok",
        };
        let parsed: Value = serde_json::from_str(audit_line(&entry).unwrap().trim_end()).unwrap();
        assert!(parsed["window_id"].is_null());
    }

    #[test]
    fn parse_scroll_maps_direction_to_signed_delta_and_clamps_amount() {
        assert_eq!(parse_scroll(&json!({"scroll_direction": "up"})).unwrap(), (0, -3));
        assert_eq!(parse_scroll(&json!({"scroll_direction": "down"})).unwrap(), (0, 3));
        assert_eq!(parse_scroll(&json!({"scroll_direction": "left"})).unwrap(), (-3, 0));
        assert_eq!(parse_scroll(&json!({"scroll_direction": "right"})).unwrap(), (3, 0));
        assert_eq!(
            parse_scroll(&json!({"scroll_direction": "down", "scroll_amount": 999})).unwrap(),
            (0, 30),
            "scroll_amount must clamp at 30, not error"
        );
        assert!(parse_scroll(&json!({"scroll_direction": "sideways"})).is_err());
        assert!(parse_scroll(&json!({})).is_err());
        // A NEGATIVE amount is rejected, never clamped to a silent no-op: the
        // human's card shows the requested value, so the executed operation
        // must be the requested one or nothing at all.
        let err = parse_scroll(&json!({"scroll_direction": "down", "scroll_amount": -5}))
            .expect_err("a negative scroll_amount must be rejected before approval");
        assert!(err.contains("must not be negative"), "{err}");
    }

    /// an ABSENT
    /// `scroll_amount` defaults to 3, but a PRESENT-but-invalid one (a JSON
    /// string, or a number outside `i64`) is REJECTED — never silently
    /// substituted with 3, which would let the approval card and the actual
    /// scroll distance disagree.
    #[test]
    fn parse_scroll_rejects_a_present_but_invalid_amount_but_defaults_when_absent() {
        // Absent → default 3 (unchanged).
        assert_eq!(parse_scroll(&json!({"scroll_direction": "down"})).unwrap(), (0, 3));
        assert_eq!(
            parse_scroll(&json!({"scroll_direction": "down", "scroll_amount": null})).unwrap(),
            (0, 3),
            "an explicit null is treated as absent → default"
        );
        // Present but not an i64 → rejected, not silently defaulted.
        assert!(
            parse_scroll(&json!({"scroll_direction": "down", "scroll_amount": "30"})).is_err(),
            "a string scroll_amount must be rejected, not defaulted to 3"
        );
        assert!(
            parse_scroll(&json!({"scroll_direction": "down", "scroll_amount": 9_999_999_999_999_999_999u64}))
                .is_err(),
            "a number above i64::MAX must be rejected, not defaulted to 3"
        );
        // A valid integer still parses (and clamps) as before.
        assert_eq!(parse_scroll(&json!({"scroll_direction": "down", "scroll_amount": 5})).unwrap(), (0, 5));
    }

    /// a call rejected as
    /// `Busy` by a DIFFERENT live holder must NOT consume a throttle slot on
    /// the way out — otherwise a foreign session polling every `THROTTLE_MS`
    /// keeps bumping the global throttle and starves the real holder.
    #[test]
    fn acquire_and_throttle_leaves_the_throttle_untouched_on_a_foreign_busy() {
        let _guard = computer::process_state_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        // Let any throttle timestamp a previous test left behind age out past
        // the window, so the assertion below is about THIS call alone.
        std::thread::sleep(std::time::Duration::from_millis(600));

        // A DIFFERENT session holds the live control lease.
        computer::acquire_control(999, "foreign", None).unwrap();

        // Our call is rejected Busy...
        let err = acquire_and_throttle(1, "10", None).unwrap_err();
        assert!(err.contains("controlling the desktop"), "expected a Busy rejection, got: {err}");

        // ...and it must have left the throttle untouched: the real holder's
        // very next input can still pace immediately (a consumed slot would
        // make this `Err(RateLimited)`).
        assert!(
            computer::throttle_input().is_ok(),
            "a foreign-holder Busy must not consume a throttle slot"
        );

        computer::clear_control();
    }

    #[test]
    fn parse_duration_ms_clamps_at_5000_and_requires_the_param() {
        assert_eq!(parse_duration_ms(&json!({"duration_ms": 200})).unwrap(), 200);
        assert_eq!(
            parse_duration_ms(&json!({"duration_ms": 999_999})).unwrap(),
            5000,
            "duration_ms must clamp at 5000, not error"
        );
        assert!(parse_duration_ms(&json!({})).is_err());
    }

    #[test]
    fn parse_coordinate_requires_two_non_negative_integers() {
        assert_eq!(parse_coordinate(&json!({"coordinate": [10, 20]}), "coordinate").unwrap(), (10, 20));
        assert!(parse_coordinate(&json!({}), "coordinate").is_err());
        assert!(parse_coordinate(&json!({"coordinate": [10]}), "coordinate").is_err());
        assert!(parse_coordinate(&json!({"coordinate": [-1, 20]}), "coordinate").is_err());
    }

    /// a JSON integer that fits `i64` but overflows
    /// `u32` (here `2^32` exactly) used to wrap `as u32` straight to `0` —
    /// silently passing the "non-negative" check and landing on a real,
    /// in-bounds-looking coordinate instead of being rejected. Covers both
    /// `x` and `y`, and both `coordinate` and `start_coordinate` share this
    /// same helper (`key` is just a label), so one fix covers both call
    /// sites in `run_action`.
    #[test]
    fn parse_coordinate_rejects_values_above_u32_max_without_wrapping() {
        let err = parse_coordinate(&json!({"coordinate": [4_294_967_296i64, 0]}), "coordinate").unwrap_err();
        assert!(err.contains("non-negative"), "{err}");
        let err_y = parse_coordinate(&json!({"coordinate": [0, 4_294_967_296i64]}), "coordinate").unwrap_err();
        assert!(err_y.contains("non-negative"), "{err_y}");
        let err_start = parse_coordinate(&json!({"start_coordinate": [4_294_967_296i64, 0]}), "start_coordinate")
            .unwrap_err();
        assert!(err_start.contains("non-negative"), "{err_start}");
        // The last value that DOES fit must still be accepted.
        assert_eq!(
            parse_coordinate(&json!({"coordinate": [u32::MAX as i64, 0]}), "coordinate").unwrap(),
            (u32::MAX, 0)
        );
    }

    // —— `required_window` ——

    #[test]
    fn required_window_accepts_a_non_blank_string() {
        assert_eq!(required_window(&json!({"window": "notes"})).unwrap(), "notes");
        // Leading/trailing whitespace around otherwise-real content is fine —
        // only ALL-whitespace (or absence) is rejected.
        assert_eq!(required_window(&json!({"window": " notes "})).unwrap(), " notes ");
    }

    #[test]
    fn required_window_rejects_missing_non_string_empty_and_blank() {
        assert!(required_window(&json!({})).is_err(), "missing key");
        assert!(required_window(&json!({"window": 5})).is_err(), "non-string");
        assert!(required_window(&json!({"window": ""})).is_err(), "empty string");
        assert!(required_window(&json!({"window": "   "})).is_err(), "all-whitespace string");
        assert!(required_window(&json!({"window": null})).is_err(), "null");
    }

    // —— audit redaction ——

    #[test]
    fn redact_audit_args_replaces_type_text_with_a_char_count_only() {
        let args = json!({"action": "type", "window": "notes", "text": "hunter2"});
        let redacted = redact_audit_args("type", &args);
        assert_eq!(redacted["window"], "notes", "non-text keys pass through untouched");
        assert_eq!(redacted["text"]["text_redacted"], true);
        assert_eq!(redacted["text"]["text_chars"], 7);
        assert!(
            redacted.to_string().contains("hunter2") == false,
            "the raw text must never appear anywhere in the redacted payload: {redacted}"
        );
    }

    #[test]
    fn redact_audit_args_counts_unicode_scalars_not_bytes() {
        // "héllo" — the "é" is 2 bytes in UTF-8 but 1 char; the redacted count
        // must match what a human reading "N characters" would expect, not
        // the raw byte length.
        let args = json!({"action": "type", "text": "héllo"});
        let redacted = redact_audit_args("type", &args);
        assert_eq!(redacted["text"]["text_chars"], 5);
    }

    #[test]
    fn redact_audit_args_leaves_key_combos_and_other_actions_untouched() {
        // A real `key` COMBO is a shortcut, not content — it stays in the audit
        // for forensics.
        let combo = json!({"action": "key", "window": "notes", "text": "cmd+s"});
        assert_eq!(redact_audit_args("key", &combo), combo, "a key combo is not redacted");
        // A non-type/non-key action is passed through wholesale.
        let click = json!({"action": "left_click", "window": "notes", "coordinate": [1, 2]});
        assert_eq!(redact_audit_args("left_click", &click), click, "other actions untouched");
    }

    /// a BARE printable `key`
    /// payload (the char-by-char text-entry case `pure_validate` rejects) is
    /// redacted in the durable audit too, so even the rejected attempt records
    /// no raw character.
    #[test]
    fn redact_audit_args_redacts_a_bare_printable_key() {
        let args = json!({"action": "key", "window": "notes", "text": "h"});
        let redacted = redact_audit_args("key", &args);
        assert_eq!(redacted["text"]["text_redacted"], true);
        assert_eq!(redacted["text"]["text_chars"], 1);
        assert_eq!(redacted["window"], "notes", "non-text keys pass through");
    }

    /// only `type` and
    /// `key` consume `text` — every other action rejects a present one
    /// outright, before any card is built, so typing content can't ride an
    /// action whose card and dispatch never mention it.
    #[test]
    fn pure_validate_rejects_a_text_argument_on_non_typing_actions() {
        let cases = [
            ("screenshot", json!({"action": "screenshot", "window": "notes", "text": "secret"})),
            (
                "left_click",
                json!({"action": "left_click", "window": "notes", "coordinate": [1, 2], "text": "secret"}),
            ),
            ("wait", json!({"action": "wait", "duration_ms": 5, "text": "secret"})),
            ("list_windows", json!({"action": "list_windows", "text": "secret"})),
        ];
        for (action, args) in cases {
            let err = pure_validate(action, &args)
                .expect_err("a non-typing action must reject a smuggled text argument");
            assert!(err.contains("does not take"), "{action}: {err}");
        }
        // The two consumers still accept it.
        assert!(pure_validate("type", &json!({"action": "type", "window": "n", "text": "hi"})).is_ok());
        assert!(pure_validate("key", &json!({"action": "key", "window": "n", "text": "ctrl+c"})).is_ok());
    }

    /// a `text` smuggled
    /// onto a non-typing action never reaches the durable log — the request
    /// is rejected (see the pure_validate test above), but rejected calls are
    /// audited too. The allowlist strips the pair WHOLESALE
    /// (it is an unrecognized argument for that action — stronger than the
    /// original char-count redaction). An UNPARSEABLE `key` payload — where
    /// `text` IS the consumed field — keeps the earlier char-count
    /// redaction: it is not a command chord forensics needs.
    #[test]
    fn redact_audit_args_redacts_smuggled_text_on_any_action() {
        let args = json!({"action": "screenshot", "window": "notes", "text": "hunter2"});
        let redacted = redact_audit_args("screenshot", &args);
        assert!(
            redacted.get("text").is_none(),
            "an unconsumed text must be stripped wholesale: {redacted}"
        );
        assert_eq!(redacted["unrecognized_args_redacted"], 1);
        assert_eq!(redacted["window"], "notes", "allowlisted keys pass through");
        assert!(
            !redacted.to_string().contains("hunter2"),
            "the raw smuggled text must never reach the audit: {redacted}"
        );

        let bad_key = json!({"action": "key", "window": "notes", "text": "hunter2"});
        let redacted = redact_audit_args("key", &bad_key);
        assert_eq!(
            redacted["text"]["text_redacted"], true,
            "an unparseable key payload is redacted, never kept raw: {redacted}"
        );
    }

    /// duplicate-shift
    /// printable chords (`shift+shift+h`) are the same text entry as
    /// `shift+h` — the semantic [`is_printable_text_chord`] predicate rejects
    /// AND redacts them regardless of how many Shift tokens pad the chord,
    /// while shift+NAMED-key and any chord carrying a non-shift modifier stay
    /// accepted commands.
    #[test]
    fn duplicate_shift_printable_chords_are_rejected_and_redacted() {
        let key = |text: &str| json!({"action": "key", "window": "notes", "text": text});
        for chord in ["shift+shift+h", "shift+shift+shift+h"] {
            assert!(
                pure_validate("key", &key(chord)).is_err(),
                "{chord} is printable text entry padded with duplicate shifts — must be rejected"
            );
            let redacted = redact_audit_args("key", &key(chord));
            assert_eq!(
                redacted["text"]["text_redacted"], true,
                "{chord} must be redacted in the audit line too: {redacted}"
            );
        }
        // Duplicate shifts on a NAMED key, and any non-shift modifier, are
        // still command chords — accepted and kept readable for forensics.
        assert!(pure_validate("key", &key("shift+shift+tab")).is_ok());
        assert!(pure_validate("key", &key("ctrl+shift+shift+t")).is_ok());
        let combo = redact_audit_args("key", &key("ctrl+shift+shift+t"));
        assert_eq!(combo["text"], "ctrl+shift+shift+t");
    }

    /// the `key`
    /// action rejects a bare printable character (use `type`) and a bare Escape
    /// (kill-switch collision), but still accepts named keys and modifier chords.
    /// windowless actions
    /// reject a smuggled `window` argument BEFORE any card is built — an
    /// accepted-but-ignored one let the card summary read as scoped
    /// (`computer: list_windows @ Calculator`) while dispatch enumerated the
    /// whole desktop.
    #[test]
    fn pure_validate_rejects_a_window_argument_on_windowless_actions() {
        for action in ["list_windows", "cursor_position"] {
            let err = pure_validate(action, &json!({"action": action, "window": "Calculator"}))
                .expect_err("a windowless action must reject a window argument");
            assert!(err.contains("does not take"), "{err}");
            assert!(pure_validate(action, &json!({"action": action})).is_ok());
        }
        let err = pure_validate("wait", &json!({"action": "wait", "duration_ms": 5, "window": "x"}))
            .expect_err("wait must reject a window argument");
        assert!(err.contains("does not take"), "{err}");
        assert!(pure_validate("wait", &json!({"action": "wait", "duration_ms": 5})).is_ok());
    }

    /// EVERY argument
    /// outside the action's closed allowlist is rejected before any card, the
    /// error never echoes the smuggled key or value (it becomes the audited
    /// outcome), and the rejected attempt's own audit line strips the pair
    /// wholesale — only a count survives.
    #[test]
    fn pure_validate_rejects_unrecognized_arguments_and_the_audit_strips_them() {
        let args = json!({"action": "screenshot", "window": "Notes", "password": "hunter2"});
        let err =
            pure_validate("screenshot", &args).expect_err("an unknown argument must be rejected");
        assert!(
            !err.contains("password") && !err.contains("hunter2"),
            "the error must not echo the smuggled key or value: {err}"
        );

        let redacted = redact_audit_args("screenshot", &args);
        assert!(redacted.get("password").is_none(), "{redacted}");
        assert_eq!(redacted["unrecognized_args_redacted"], 1);
        assert_eq!(redacted["window"], "Notes", "allowlisted keys pass through");
        let rendered = redacted.to_string();
        assert!(
            !rendered.contains("hunter2") && !rendered.contains("password"),
            "neither the smuggled key nor its value may persist: {rendered}"
        );

        // A fully-allowlisted request is untouched by both boundaries.
        let clean = json!({"action": "screenshot", "window": "Notes"});
        assert!(pure_validate("screenshot", &clean).is_ok());
        assert_eq!(redact_audit_args("screenshot", &clean), clean);
    }

    #[test]
    fn pure_validate_rejects_bare_printable_and_bare_escape_key() {
        let key = |text: &str| json!({"action": "key", "window": "notes", "text": text});
        assert!(pure_validate("key", &key("a")).is_err(), "a bare printable char is rejected");
        assert!(pure_validate("key", &key("Escape")).is_err(), "a bare Escape is rejected");
        assert!(pure_validate("key", &key("esc")).is_err(), "the `esc` alias is rejected too");
        // Named keys, modifier chords, and modified-Escape chords still pass.
        assert!(pure_validate("key", &key("enter")).is_ok());
        assert!(pure_validate("key", &key("ctrl+c")).is_ok());
        assert!(pure_validate("key", &key("shift+escape")).is_ok());
    }

    /// any chord that would press
    /// Escape UNMODIFIED is rejected semantically — a held Escape
    /// (`escape+escape`, `escape+a`) or an Escape clicked with only
    /// non-modifier keys held (`tab+escape`) collides with the global
    /// emergency-stop shortcut exactly like a bare `escape` press,
    /// while genuinely modified chords stay accepted.
    #[test]
    fn unmodified_escape_chords_are_rejected_semantically() {
        let key = |text: &str| json!({"action": "key", "window": "notes", "text": text});
        for chord in ["escape+escape", "escape+a", "tab+escape"] {
            assert!(
                pure_validate("key", &key(chord)).is_err(),
                "{chord} injects an unmodified Escape press — must be rejected"
            );
        }
        // Real-modifier chords keep Escape's press modified — still accepted.
        assert!(pure_validate("key", &key("shift+escape")).is_ok());
        assert!(pure_validate("key", &key("ctrl+escape")).is_ok());
    }

    /// a SHIFT-ONLY
    /// printable chord (`shift+h` = `H`) is char-by-char TEXT entry one shift
    /// away from the bare case above — rejected (use `type`) AND redacted in
    /// the audit line, while command chords (`ctrl+h`, `shift+tab`,
    /// `ctrl+shift+t`) stay accepted and unredacted for forensics.
    #[test]
    fn shift_only_printable_key_is_rejected_and_redacted_like_bare_text() {
        let key = |text: &str| json!({"action": "key", "window": "notes", "text": text});
        assert!(
            pure_validate("key", &key("shift+h")).is_err(),
            "shift+printable is text entry — must be rejected toward `type`"
        );
        // Non-shift modifier chords and shift+NAMED-key chords still pass.
        assert!(pure_validate("key", &key("ctrl+h")).is_ok());
        assert!(pure_validate("key", &key("shift+tab")).is_ok());
        assert!(pure_validate("key", &key("ctrl+shift+t")).is_ok());

        // Audit redaction mirrors the same boundary: even the rejected
        // attempt's audit line never records the raw shifted character.
        let redacted = redact_audit_args("key", &key("shift+h"));
        assert_eq!(redacted["text"]["text_redacted"], true, "{redacted}");
        // A command chord stays readable in the audit.
        let combo = redact_audit_args("key", &key("ctrl+shift+t"));
        assert_eq!(combo["text"], "ctrl+shift+t", "{combo}");
    }

    #[test]
    fn redact_audit_args_leaves_a_missing_text_key_missing() {
        // A malformed `type` call missing `text` entirely never reaches the
        // backend (`required_text` rejects it first) — its audit record must
        // reflect the REAL request, not manufacture a `text` key that was
        // never actually sent.
        let args = json!({"action": "type", "window": "notes"});
        let redacted = redact_audit_args("type", &args);
        assert_eq!(redacted, args);
        assert!(redacted.get("text").is_none());
    }

    // —— detail_redacted (the IM-bridge leak) ——

    /// The end-to-end property the fix exists for: a `type` action's Ask
    /// keeps the RAW text in `detail` (the LOCAL desktop card still needs it
    /// to judge the approval) but ALSO carries a `detail_redacted` (what
    /// `im::outbound::perm_card` must show instead) that never contains the
    /// raw text — the same shape `redact_audit_args` already gives the
    /// durable audit line.
    #[tokio::test]
    async fn approve_sets_detail_redacted_for_type_but_keeps_the_local_detail_raw() {
        // Note: `approve` resolves a Write
        // action's window AUTHORITATIVELY before it even checks a standing
        // grant (so it can fold the identity into `action_key`) — this needs
        // a resolvable "notes" window (`shared_mock`, under
        // `process_state_test_lock`) so that early resolve itself succeeds,
        // rather than failing before a card is ever created.
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 907_001,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 907_001;
        let dir = "lead";
        let args = json!({"action": "type", "window": "notes", "text": "hunter2"});

        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move { approve(&asks_bg, thread, dir, None, "type", &args).await });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect("a Needs-you card must appear for a type action");

        assert!(card.detail.contains("hunter2"), "the LOCAL card must keep the raw text: {card:?}");
        let redacted = card
            .detail_redacted
            .as_deref()
            .expect("a type action's Ask must carry detail_redacted");
        assert!(
            !redacted.contains("hunter2"),
            "detail_redacted must never contain the raw typed text: {redacted}"
        );
        assert!(redacted.contains("text_redacted"), "{redacted}");

        assert!(asks.answer(card.id, crate::ask::Answer::Deny));
        let _ = handle.await.unwrap();
    }

    /// Every action other than `type` must carry NO `detail_redacted` at
    /// all — nothing else this server's schema defines is treated as secret
    /// content, so `im::outbound::perm_card` falls back to the (already
    /// harmless) full `detail` for these.
    #[tokio::test]
    async fn approve_leaves_detail_redacted_none_for_a_non_type_action() {
        // Note: see the matching comment in
        // `approve_sets_detail_redacted_for_type_but_keeps_the_local_detail_raw`
        // above — `left_click` is Write-classified too, so it needs the same
        // resolvable-window setup for `approve`'s own early resolve.
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 907_002,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 907_002;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "notes", "coordinate": [1, 1]});

        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move { approve(&asks_bg, thread, dir, None, "left_click", &args).await });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect("a Needs-you card must appear for a left_click action");
        assert!(card.detail_redacted.is_none(), "{card:?}");

        assert!(asks.answer(card.id, crate::ask::Answer::Deny));
        let _ = handle.await.unwrap();
    }

    // —— GUI actions never sweep in the
    // generic read-only batch/issue grant ——

    /// The end-to-end property the fix exists for: a session already holding
    /// the "release all read-only for this session" grant — the
    /// GENERIC, cross-tool batch grant, not anything computer-specific — must
    /// still see a real Needs-you card for a `screenshot`/`list_windows`
    /// call, because those are `RiskLevel::ReadOnly` by `classify_gui_action`'s
    /// own construction REGARDLESS of which window they target. Before this
    /// fix, `approve` called the ordinary `auto_decision` (which DOES honor
    /// that grant for any ReadOnly ask) and would have silently captured
    /// pixels/enumerated windows with no card and no computer-specific
    /// Always ever granted.
    #[tokio::test]
    async fn screenshot_still_cards_despite_a_read_only_session_grant_with_no_exact_always() {
        // Note: `approve` resolves a
        // `screenshot`'s window authoritatively too (not just Write actions),
        // even on the path to opening a card — this needs a resolvable
        // "notes" window (`shared_mock`, under `process_state_test_lock`) so
        // that resolve itself succeeds, rather than failing before a card is
        // ever created.
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 908_001,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 908_001;
        let dir = "lead";
        asks.grant_read_only_session(thread, dir);

        let args = json!({"action": "screenshot", "window": "notes"});
        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move { approve(&asks_bg, thread, dir, None, "screenshot", &args).await });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect(
            "a screenshot must still surface a Needs-you card despite the read-only-session grant \
             — it must never be swept in by the generic read-only batch grant",
        );
        assert_eq!(card.risk, crate::ask::RiskLevel::ReadOnly, "{card:?}");

        assert!(asks.answer(card.id, crate::ask::Answer::Deny));
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.contains("denied"), "{err}");
    }

    /// Same property for the ISSUE-wide read-only grant
    /// — `list_windows` must still card too.
    #[tokio::test]
    async fn list_windows_still_cards_despite_a_read_only_issue_grant() {
        let asks = AskRegistry::new();
        let thread = 908_002;
        let dir = "lead";
        asks.grant_read_only_issue(thread);

        let args = json!({"action": "list_windows"});
        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move { approve(&asks_bg, thread, dir, None, "list_windows", &args).await });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect(
            "list_windows must still surface a Needs-you card despite the read-only-issue grant",
        );
        assert_eq!(card.risk, crate::ask::RiskLevel::ReadOnly, "{card:?}");

        assert!(asks.answer(card.id, crate::ask::Answer::Deny));
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.contains("denied"), "{err}");
    }

    /// The other half of the property: a session with NO grant at all except
    /// a precise, EXACT `action_key` Always grant for this SAME
    /// `screenshot @ notes` call is still auto-approved without a card —
    /// `auto_decision_gui` keeps honoring Full/Always exactly like
    /// `auto_decision` does, it just drops the read-only batch/issue fallback
    /// (and the global `dangerous` shortcut). Proves the fix is a
    /// narrowing, not a blanket "GUI actions never auto-approve".
    #[tokio::test]
    async fn screenshot_auto_approves_with_an_exact_always_grant_and_no_read_only_batch() {
        // `screenshot` now resolves its window
        // authoritatively too, folding `id`/`app`/`title` into the key
        //  exactly like a Write action's key —
        // this pre-seeded Always grant must be built the SAME way `approve`
        // itself now would, or it silently misses and this call hangs
        // waiting on a card nobody is spawned to answer.
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 908_003,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 908_003;
        let dir = "lead";
        let args = json!({"action": "screenshot", "window": "notes"});
        let action_key = crate::ask::action_key(&[
            "gui",
            "screenshot",
            "notes",
            "908003",
            "Notes",
            "notes",
            &args_digest(&args),
        ]);
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: Vec::new(),
            always: vec![crate::ask::AlwaysGrant { thread, dir: dir.to_string(), action_key }],
        });

        // No card ever appears — this must resolve on its own, promptly.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            approve(&asks, thread, dir, None, "screenshot", &args),
        )
        .await
        .expect("an exact Always grant must auto-approve without ever needing a human answer");
        assert!(result.is_ok(), "{result:?}");
        assert!(asks.open().is_empty(), "an auto-approved call must never surface a card");
    }

    // —— type/key focus-freshness gate ——
    //
    // Each test below uses a UNIQUE synthetic thread id so they can run in
    // parallel (the default for `cargo test`) without racing each other on
    // the shared process-level `recent_clicks()` registry.

    /// A minimal window fixture for the focus tests — the
    /// registry is identity-keyed, so the tests pass full `WindowInfo`s.
    fn focus_win(id: u32) -> computer::WindowInfo {
        computer::WindowInfo {
            id,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }
    }

    #[test]
    fn require_recent_focus_passes_right_after_a_click_on_the_same_window() {
        let thread = 900_001;
        record_click_focus(thread, "lead", None, &focus_win(7));
        assert!(require_recent_focus(thread, "lead", None, &focus_win(7)).is_ok());
    }

    #[test]
    fn require_recent_focus_rejects_with_no_prior_click_at_all() {
        let thread = 900_002;
        let err = require_recent_focus(thread, "lead", None, &focus_win(7)).unwrap_err();
        assert!(err.contains("focus"), "{err}");
        assert!(err.contains("click"), "{err}");
    }

    #[test]
    fn require_recent_focus_rejects_a_click_on_a_different_window() {
        let thread = 900_003;
        record_click_focus(thread, "lead", None, &focus_win(7)); // clicked window A (id 7)
        let err = require_recent_focus(thread, "lead", None, &focus_win(8)).unwrap_err(); // typing into B (id 8)
        assert!(err.contains("8"), "error should name the window that lacks focus: {err}");
    }

    /// a REUSED numeric
    /// id must not satisfy the freshness check — the clicked window closed
    /// and a different app/title took its number within the 15s window.
    #[test]
    fn require_recent_focus_rejects_a_reused_id_with_a_different_identity() {
        let thread = 900_007;
        record_click_focus(thread, "lead", None, &focus_win(7));
        let mut imposter = focus_win(7);
        imposter.app = "Mail".into();
        imposter.title = "inbox".into();
        assert!(
            require_recent_focus(thread, "lead", None, &imposter).is_err(),
            "the SAME id with a different app/title must not read as recently clicked"
        );
        // The genuine identity still passes.
        assert!(require_recent_focus(thread, "lead", None, &focus_win(7)).is_ok());
    }

    #[test]
    fn require_recent_focus_is_scoped_per_thread_dir() {
        let thread_a = 900_004;
        let thread_b = 900_005;
        record_click_focus(thread_a, "lead", None, &focus_win(7));
        // A click recorded for a DIFFERENT (thread, dir) must not satisfy
        // this one's focus check — the registry is per-session, not global.
        assert!(require_recent_focus(thread_b, "lead", None, &focus_win(7)).is_err());
        assert!(require_recent_focus(thread_a, "10", None, &focus_win(7)).is_err());
    }

    /// sibling workers of
    /// one multi-repo direction share `(thread, dir)` but are distinct
    /// sessions keyed by `wt` — worker A's click must never satisfy worker
    /// B's `type`/`key` focus prerequisite, nor the lead-style `None`
    /// resolution's; the owner still passes.
    #[test]
    fn require_recent_focus_is_isolated_per_worktree_for_sibling_workers() {
        let thread = 900_007;
        record_click_focus(thread, "40", Some(1), &focus_win(7));
        assert!(
            require_recent_focus(thread, "40", Some(2), &focus_win(7)).is_err(),
            "a sibling worker differing only by wt must not inherit the click"
        );
        assert!(
            require_recent_focus(thread, "40", None, &focus_win(7)).is_err(),
            "the wt-less resolution must not inherit a worker's click either"
        );
        assert!(require_recent_focus(thread, "40", Some(1), &focus_win(7)).is_ok());
    }

    #[test]
    fn require_recent_focus_rejects_once_the_freshness_window_has_expired() {
        let thread = 900_006;
        // Seed a click stamped older than `FOCUS_FRESHNESS_MS` directly,
        // rather than sleeping 15s in a test — same "no fake clock needed"
        // approach the coordinator's spec calls for, just expressed as a
        // pre-expired `Instant` (the registry is monotonic
        // Instant-based) instead of a real-time wait. `checked_sub` can
        // only return `None` if the process somehow started less than ~15s
        // after the Instant epoch — skip (vacuously pass) rather than panic
        // in that unreachable-in-practice case.
        let Some(stale) = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(FOCUS_FRESHNESS_MS + 1_000))
        else {
            return;
        };
        {
            let mut g = recent_clicks().lock().unwrap();
            g.insert(
                (thread, "lead".to_string(), None),
                (VerifiedWindowIdentity::from_window(&focus_win(7)), stale),
            );
        }
        assert!(require_recent_focus(thread, "lead", None, &focus_win(7)).is_err());
    }

    // —— activate_target ——

    /// The ONE shared `MockBackend` every test in this module that needs to
    /// drive `run_action` through the process-wide `backend::backend()`
    /// singleton installs — `backend::_set_backend_override` is a
    /// set-ONCE-per-process `OnceLock` (see its own doc comment), so a
    /// second test calling it with a DIFFERENT `MockBackend` instance would
    /// just silently keep using whichever one happened to install first.
    /// Every such test calls THIS helper instead of installing its own, then
    /// configures the SAME instance's interior-mutable fields
    /// (`windows_override`, `actions`, `fail_activate`, `on_activate`) for
    /// its own scenario — resetting whatever it depends on at the START of
    /// its own test body (never relying on a previous test's cleanup), and
    /// under `computer::process_state_test_lock` like every other test in
    /// this file that touches shared, un-keyed process state (the mock
    /// backend override is exactly that kind of state once installed).
    fn shared_mock() -> std::sync::Arc<computer::mock::MockBackend> {
        static MOCK: OnceLock<std::sync::Arc<computer::mock::MockBackend>> = OnceLock::new();
        MOCK.get_or_init(|| {
            let mock = std::sync::Arc::new(computer::mock::MockBackend::default());
            backend::_set_backend_override(mock.clone());
            mock
        })
        .clone()
    }

    /// The end-to-end property this fix exists for:
    /// `activate_target` reactivates the target window
    /// (`backend.activate_window`) UNCONDITIONALLY, every time it's called —
    /// there is no longer an Auto/Interactive distinction to skip it for (see
    /// this function's own doc for why that distinction was removed). Also covers the fail-closed path: when activation itself
    /// is broken (`Unsupported`), this must propagate an `Err` naming the
    /// window, never fall through and let the real action reach the OS
    /// anyway.
    #[test]
    fn activate_target_always_activates_and_fails_closed_when_unsupported() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        // activation verifies the FULL identity
        // against the currently visible set, so the target must be visible.
        let target = computer::WindowInfo {
            id: 7,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        };
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(vec![target.clone()]);

        // Every call activates the EXACT target window id, unconditionally.
        assert!(activate_target(&target).is_ok());
        {
            let actions = mock.actions.lock().unwrap();
            assert_eq!(actions.len(), 1, "{actions:?}");
            assert_eq!(actions[0], "activate 7", "{actions:?}");
        }

        // A second call activates again — this is no longer gated on
        // "did a card actually appear", so repeated calls each activate.
        assert!(activate_target(&target).is_ok());
        assert_eq!(mock.actions.lock().unwrap().len(), 2);

        // the SAME
        // numeric id now belonging to a DIFFERENT application (id reuse while
        // the activation closure sat queued) is refused BEFORE any raise —
        // no new action recorded.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(vec![computer::WindowInfo {
                id: 7,
                app: "Imposter".into(),
                title: "imposter".into(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            }]);
        let err = activate_target(&target).unwrap_err();
        assert!(err.contains('7'), "{err}");
        assert_eq!(
            mock.actions.lock().unwrap().len(),
            2,
            "an id-reuse mismatch must refuse before the raise, recording nothing"
        );
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(vec![target.clone()]);

        // Fail-closed: the backend can't activate the window at all
        // (`Unsupported`) — must propagate an `Err` naming the window,
        // never silently proceed. No NEW action is recorded (the count
        // stays at 2, from the two successful activations above).
        mock.fail_activate.store(true, std::sync::atomic::Ordering::SeqCst);
        let err = activate_target(&target).unwrap_err();
        assert!(err.contains('7'), "{err}");
        assert_eq!(
            mock.actions.lock().unwrap().len(),
            2,
            "a failed activation must never itself be recorded as a successful action"
        );

        // Leave clean for the next test sharing this mock instance.
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    // —— input branches re-resolve/re-activate AFTER the flight guard ——

    /// a `left_click` that queues on
    /// `computer::input_flight_guard` behind another in-flight action must
    /// use the window's geometry AS IT IS once it actually gets to inject —
    /// not whatever `resolve_window`/`map_to_physical` would have computed
    /// before it ever reached the guard. This test holds the guard itself
    /// (standing in for "another session's own in-flight call"), spawns the
    /// click, moves the mock window's origin while the click sits queued
    /// behind it, then releases the guard and asserts the injected
    /// coordinates reflect the NEW origin, never the stale one that was
    /// current when the call started.
    #[tokio::test]
    async fn left_click_uses_the_windows_geometry_as_of_after_the_flight_guard_not_before() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 906_301,
            app: "Baz".into(),
            title: "Baz".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 906_301;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });
        // an input action now maps its coordinate
        // against a RECORDED screenshot's own dimensions (fail-closed with
        // none on file) rather than the window's current size — seed the
        // record directly (standing in for "this session already
        // screenshotted this window") since the CURRENT size is what this
        // test's own window origin/size started at, at 1:1 scale.
        computer::record_shot_dims(
            thread,
            dir,
            None,
            906_301,
            800,
            600,
            &computer::WindowInfo {
                id: 906_301,
                app: "Baz".into(),
                title: "Baz".into(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
        );

        // Prime the global input throttle well ahead of time so the real
        // click below isn't itself rejected as rate-limited.
        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // Hold the flight guard OURSELVES — the click below must queue
        // behind it, exactly like a second `tools/call` racing an in-flight
        // one would.
        let held = computer::input_flight_guard().await;

        let db_bg = db.clone();
        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move {
            let mut window_id_out = None;
            let mut image_out = None;
            run_action(
                &db_bg, &asks_bg, thread, dir, None, session_token_generation(thread, dir, None), "computer", "left_click",
                &json!({"window": "Baz", "coordinate": [100, 50]}),
                &mut window_id_out, &mut image_out,
            )
            .await
        });

        // Give the spawned call a chance to run its own pre-guard work
        // (argument parsing, `check_suspended`, `acquire_and_throttle`) and
        // reach — and block on — the flight guard THIS test still holds.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The window "moves" while the click sits queued.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 906_301,
            app: "Baz".into(),
            title: "Baz".into(),
            x: 500,
            y: 300,
            width: 800,
            height: 600,
        }]);

        drop(held);

        let result = handle.await.unwrap();
        assert!(result.is_ok(), "{result:?}");

        let actions = mock.actions.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            actions.iter().any(|a| a == "click 600,350 Left x1"),
            "the click must land at the window's NEW origin (500+100, 300+50), not a stale one \
             computed before the flight guard: {actions:?}"
        );
        assert!(
            !actions.iter().any(|a| a == "click 100,50 Left x1"),
            "must never use the STALE pre-guard coordinates: {actions:?}"
        );
        drop(actions);

        computer::clear_control();
    }

    /// `activate_target` shells out to a
    /// (potentially slow, blocking) OS call. A Stop that lands WHILE that
    /// call is running must still be honored: the post-activation recheck
    /// (inside `pace_activate_verify_and_inject`'s paced closure, right
    /// after activation) must reject, and the backend must NEVER receive the
    /// `type_text` call. Reproduced deterministically via
    /// `MockBackend::on_activate`, which runs synchronously from INSIDE the
    /// mock's own `activate_window` — standing in for "a human's Stop
    /// finished processing while the real, blocking activation call was
    /// still running" — so by the time `activate_window` returns, its
    /// effect (here: `computer::clear_control()`, exactly what a real
    /// `emergency_stop` also does) has already happened, without needing
    /// genuine OS-thread concurrency to "catch" the race.
    #[tokio::test]
    async fn type_is_rejected_by_the_second_recheck_if_the_control_lease_is_lost_during_activation() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 906_201,
            app: "Bar".into(),
            title: "Bar".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 906_201;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        // Focus-freshness: `type` requires a recent click on the SAME
        // window first (see `require_recent_focus`) — seed it directly
        // rather than driving a real `left_click` through the whole gate a
        // second time just to satisfy this precondition.
        record_click_focus(thread, dir, None, &computer::WindowInfo {
            id: 906_201,
            app: "Bar".into(),
            title: "Bar".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        });

        // Prime + clear the global input throttle so the real call below
        // isn't itself rejected as rate-limited.
        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // The hook: simulate a Stop landing DURING activation by clearing
        // the control lease from inside `activate_window` itself.
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(computer::clear_control));

        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, session_token_generation(thread, dir, None), "computer", "type",
            &json!({"window": "Bar", "text": "hello"}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(
            err.contains("lost") || err.to_lowercase().contains("busy"),
            "expected a lost-lease/busy rejection from the SECOND recheck: {err}"
        );
        let actions = mock.actions.lock().unwrap_or_else(|e| e.into_inner());
        assert!(actions.iter().any(|a| a == "activate 906201"), "{actions:?}");
        assert!(
            !actions.iter().any(|a| a.starts_with("type ")),
            "the backend must never receive the type call once the lease is lost mid-activation: {actions:?}"
        );
        drop(actions);

        // Leave clean for the next test sharing this mock instance.
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        computer::clear_control();
    }

    /// The entry capture returns exactly the generation the bearer verified
    /// at, and a later rotation makes both the old bearer (entry gate) and
    /// the old CAPTURED generation (in-flight checkpoints) fail.
    #[test]
    fn the_entry_capture_binds_a_request_to_the_generation_its_bearer_verified_at() {
        let thread = 933_201;
        let token = mint_committed(thread, "lead", None);
        let auth_gen = verify_computer_token_at_current_generation(thread, "lead", None, &token)
            .expect("a freshly minted bearer must verify");
        assert_eq!(auth_gen, session_token_generation(thread, "lead", None));
        assert!(verify_bearer_generation(thread, "lead", None, auth_gen).is_ok());

        // A replacement launch rotates: the old bearer no longer verifies at
        // the entry gate, and the old captured generation now fails every
        // in-flight checkpoint with the superseded message.
        let _replacement = mint_committed(thread, "lead", None);
        assert!(
            verify_computer_token_at_current_generation(thread, "lead", None, &token).is_none(),
            "the superseded bearer must 401 at the entry gate"
        );
        let err = verify_bearer_generation(thread, "lead", None, auth_gen).unwrap_err();
        assert!(err.contains("superseded"), "{err}");
    }

    /// A replacement launch's rotation landing while a call is ALREADY in
    /// flight (here: during the blocking activation shell-out, standing in
    /// for any of the long post-entry waits) must stop the injection — the
    /// entry gate only ever 401s NEW requests, so the in-flight
    /// checkpoints have to catch the superseded bearer themselves.
    /// Deterministic via `MockBackend::on_activate`, exactly like the
    /// lease-loss twin above.
    #[tokio::test]
    async fn type_is_rejected_when_the_bearer_is_superseded_during_activation() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 906_401,
            app: "Bar".into(),
            title: "Bar".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 906_401;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });
        record_click_focus(thread, dir, None, &computer::WindowInfo {
            id: 906_401,
            app: "Bar".into(),
            title: "Bar".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        });
        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // The request authenticated at THIS generation...
        let auth_gen = session_token_generation(thread, dir, None);
        // ...and a replacement launch rotates it while the activation
        // shell-out is running.
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(move || {
            let _ = mint_committed(906_401, "lead", None);
        }));

        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, auth_gen, "computer", "type",
            &json!({"window": "Bar", "text": "hello"}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(err.contains("superseded"), "{err}");
        let actions = mock.actions.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !actions.iter().any(|a| a.starts_with("type ")),
            "the backend must never receive the type call once the bearer is superseded: {actions:?}"
        );
        drop(actions);

        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        computer::clear_control();
    }

    // —— re-resolve/re-verify AFTER activation ——

    /// The end-to-end property this fix exists for: while `activate_window`
    /// runs (standing in for a real, slow OS activation call), the window
    /// moves to a NEW origin — SAME id/app/title, so `verify_approved_target`
    /// still passes; only the GEOMETRY changed. The click must land using the
    /// window's geometry AS OF AFTER activation, never the stale
    /// pre-activation origin.
    ///
    /// Note: this origin change is also exactly
    /// why `computer::shot_dims_for` does NOT gate on geometry — see that
    /// function's own doc, which names this test by function name as one of
    /// the two reasons.
    #[tokio::test]
    async fn left_click_uses_the_windows_geometry_as_of_after_activation_not_before() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 910_401,
            app: "Moving".into(),
            title: "moving window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 910_401;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });
        // seed this window's own recorded
        // screenshot dims (its size never changes in this scenario, only its
        // origin does — see the hook below) so the click's coordinate mapping
        // doesn't fail closed for want of a screenshot on file.
        computer::record_shot_dims(
            thread,
            dir,
            None,
            910_401,
            800,
            600,
            &computer::WindowInfo {
                id: 910_401,
                app: "Moving".into(),
                title: "moving window".into(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
        );

        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // The hook: while `activate_window` runs, the window moves to a NEW
        // origin — same identity, different geometry.
        let mock_for_hook = mock.clone();
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(move || {
            *mock_for_hook.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
                id: 910_401,
                app: "Moving".into(),
                title: "moving window".into(),
                x: 400,
                y: 200,
                width: 800,
                height: 600,
            }]);
        }));

        let mut window_id_out = None;
        let mut image_out = None;
        let result = run_action(
            &db, &asks, thread, dir, None, session_token_generation(thread, dir, None), "computer", "left_click",
            &json!({"window": "moving", "coordinate": [10, 10]}),
            &mut window_id_out, &mut image_out,
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
        let actions = mock.actions.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            actions.iter().any(|a| a == "click 410,210 Left x1"),
            "the click must use the window's geometry AS OF AFTER activation \
             (400+10, 200+10), not the stale pre-activation origin: {actions:?}"
        );
        assert!(
            !actions.iter().any(|a| a == "click 10,10 Left x1"),
            "must never use the stale pre-activation origin (0,0): {actions:?}"
        );
        drop(actions);

        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        computer::clear_control();
    }

    /// The fail-closed half: while `activate_window` runs, the window is
    /// REPLACED — same query still matches (title substring), but a
    /// DIFFERENT id AND app — standing in for the original window closing
    /// and an unrelated one taking its place mid-activation. The action must
    /// fail closed (the SAME re-approve message `verify_approved_target`
    /// gives the approve→dispatch gap) and the backend must never receive
    /// the keystrokes.
    #[tokio::test]
    async fn type_fails_closed_when_the_window_is_replaced_during_activation() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 910_501,
            app: "Swappy".into(),
            title: "swappy window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 910_501;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });
        record_click_focus(thread, dir, None, &computer::WindowInfo {
            id: 910_501,
            app: "Swappy".into(),
            title: "swappy window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        });

        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // The hook: while `activate_window` runs, the window is REPLACED by
        // a different id/app that happens to share the same title substring.
        let mock_for_hook = mock.clone();
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(move || {
            *mock_for_hook.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
                id: 910_502,
                app: "Different".into(),
                title: "swappy window".into(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            }]);
        }));

        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, session_token_generation(thread, dir, None), "computer", "type",
            &json!({"window": "swappy", "text": "hello"}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(
            err.contains("changed since this action was approved"),
            "must fail closed with the re-approve message: {err}"
        );
        let actions = mock.actions.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !actions.iter().any(|a| a.starts_with("type ")),
            "the replaced window must never receive the keystrokes: {actions:?}"
        );
        drop(actions);

        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        computer::clear_control();
    }

    // —— approval bound to the resolved window identity ——

    /// The end-to-end property this fix exists for: `approve` resolves the
    /// target window (query "shifty", matching by title substring) and binds
    /// THAT window's identity at authorization time. If the SAME query
    /// resolves to a DIFFERENT window (different id AND app — simulating the
    /// original window closing and an unrelated one since taking the same
    /// title) by the time `run_action`'s own "left_click" arm reaches its OWN
    /// fresh resolve, the call must be rejected — fail-closed — and the
    /// backend must NEVER receive the click. Reproduced deterministically via
    /// the same "hold the flight guard ourselves" technique
    /// `left_click_uses_the_windows_geometry_as_of_after_the_flight_guard_not_before`
    /// uses: the window swap lands in the gap between `approve`'s own bind
    /// and the arm's later re-resolve.
    #[tokio::test]
    async fn input_action_fails_closed_when_the_approved_window_is_replaced_before_dispatch() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 907_401,
            app: "Shifty".into(),
            title: "shifty window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 907_401;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        // Prime the global input throttle well ahead of time so the real
        // click below isn't itself rejected as rate-limited.
        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // Hold the flight guard OURSELVES — `approve`'s own bind runs
        // (synchronously, no await point of its own) BEFORE the spawned call
        // ever reaches this guard, so it captures the window as it is RIGHT
        // NOW; the swap below then lands strictly after that bind.
        let held = computer::input_flight_guard().await;

        let db_bg = db.clone();
        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move {
            let mut window_id_out = None;
            let mut image_out = None;
            run_action(
                &db_bg, &asks_bg, thread, dir, None, session_token_generation(thread, dir, None), "computer", "left_click",
                &json!({"window": "shifty", "coordinate": [10, 10]}),
                &mut window_id_out, &mut image_out,
            )
            .await
        });

        // Give the spawned call a chance to run `approve` (which binds the
        // CURRENT window) and its own pre-guard argument checks, then reach
        // — and block on — the flight guard this test still holds.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The window is REPLACED: the SAME query still matches (same title
        // substring), but a DIFFERENT id and app — standing in for the
        // original window closing and an unrelated one happening to share
        // its name.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 907_402,
            app: "Different App".into(),
            title: "shifty window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        drop(held);

        let err = handle.await.unwrap().unwrap_err();
        assert!(
            err.contains("changed since this action was approved"),
            "must fail closed with the re-approve message: {err}"
        );
        assert!(
            mock.actions.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "the replaced window must never receive the click"
        );

        computer::clear_control();
    }

    /// The mirror image: the query resolves to the SAME window both at
    /// approval time and at dispatch time (the common, no-race case) — the
    /// action must still succeed exactly as it did without the binding.
    #[tokio::test]
    async fn input_action_succeeds_when_the_approved_window_is_unchanged() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 907_501,
            app: "Steady".into(),
            title: "steady window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 907_501;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });
        // seed this window's recorded screenshot
        // dims (unchanged for this scenario) so the click's coordinate
        // mapping doesn't fail closed for want of a screenshot on file.
        computer::record_shot_dims(
            thread,
            dir,
            None,
            907_501,
            800,
            600,
            &computer::WindowInfo {
                id: 907_501,
                app: "Steady".into(),
                title: "steady window".into(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
        );

        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        let mut window_id_out = None;
        let mut image_out = None;
        let result = run_action(
            &db, &asks, thread, dir, None, session_token_generation(thread, dir, None), "computer", "left_click",
            &json!({"window": "steady", "coordinate": [5, 5]}),
            &mut window_id_out, &mut image_out,
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(window_id_out, Some(907_501));
        assert!(
            mock.actions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|a| a.starts_with("click")),
            "the unchanged window must still receive the click"
        );

        computer::clear_control();
    }

    // —— standing Always grant scoped to the
    // approved window's own identity, not just the query string ——

    /// The end-to-end property this fix exists for: an Always grant seeded
    /// with a key that includes the CURRENT window's `app`+`title` still
    /// auto-approves the SAME query when it resolves to that SAME window —
    /// this is the "narrowing must not break the common case" half.
    #[tokio::test]
    async fn standing_always_grant_auto_approves_when_the_window_identity_is_unchanged() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 909_101,
            app: "Editor".into(),
            title: "notes.txt — Editor".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 909_101;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "editor", "coordinate": [1, 1]});
        let action_key = crate::ask::action_key(&[
            "gui",
            "left_click",
            "editor",
            "909101",
            "Editor",
            "notes.txt — Editor",
            &args_digest(&args),
        ]);
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: Vec::new(),
            always: vec![crate::ask::AlwaysGrant { thread, dir: dir.to_string(), action_key }],
        });

        let approved = tokio::time::timeout(std::time::Duration::from_secs(5), approve(&asks, thread, dir, None, "left_click", &args))
            .await
            .expect("must resolve without ever needing a human answer")
            .expect("an exact identity match must auto-approve");
        assert_eq!(
            approved,
            Some(ApprovedWindow { id: 909_101, app: "Editor".into(), title: "notes.txt — Editor".into() })
        );
        assert!(asks.open().is_empty(), "an auto-approved call must never surface a card");
    }

    /// The narrowing half: the SAME query now resolves to a DIFFERENT window
    /// (different id AND app — standing in for the original window closing
    /// and an unrelated one taking its place) — the standing Always grant
    /// seeded for the OLD identity must MISS (a different `action_key`,
    /// since `app`+`title` are folded into it), falling through to a fresh
    /// Needs-you card rather than silently auto-approving the replacement
    /// window. Without identity in the key, it never captured
    /// WHICH window was approved (only the query string), so this exact
    /// scenario would have auto-approved silently.
    #[tokio::test]
    async fn standing_always_grant_misses_a_different_window_identity_and_cards() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 909_201,
            app: "Editor".into(),
            title: "notes.txt — Editor".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 909_201;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "editor", "coordinate": [1, 1]});
        // Seed an Always grant for the CURRENT (soon-to-be-replaced) window's
        // identity.
        let action_key = crate::ask::action_key(&[
            "gui",
            "left_click",
            "editor",
            "909201",
            "Editor",
            "notes.txt — Editor",
            &args_digest(&args),
        ]);
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: Vec::new(),
            always: vec![crate::ask::AlwaysGrant { thread, dir: dir.to_string(), action_key }],
        });

        // The query now resolves to a DIFFERENT window.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 909_202,
            app: "OtherApp".into(),
            title: "an unrelated editor window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks_bg = asks.clone();
        let args_bg = args.clone();
        let handle = tokio::spawn(async move { approve(&asks_bg, thread, dir, None, "left_click", &args_bg).await });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect(
            "a DIFFERENT window identity must miss the standing grant's key and surface a fresh card",
        );

        assert!(asks.answer(card.id, crate::ask::Answer::Deny));
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.contains("denied"), "{err}");
    }

    // —— Always key now also binds the window INSTANCE (id) ——

    /// The id-in-the-key gap, isolated from the
    /// (broader) "different window entirely" scenario above: the ORIGINAL
    /// window closes and a NEW one opens with the EXACT SAME `app`+`title` —
    /// a relaunched app, a reopened document with an identical name — but a
    /// DIFFERENT `id`. Before this change, the key carried `app`+`title` but
    /// NOT `id`, so this replacement window's key was IDENTICAL to the
    /// original's — the standing Always grant would have silently kept
    /// authorizing input into the REPLACEMENT instance. With `id` folded in,
    /// the SAME query now mints a DIFFERENT key and must fall through to a
    /// fresh card instead.
    #[tokio::test]
    async fn standing_always_grant_misses_a_replaced_window_instance_with_the_same_app_and_title() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 909_301,
            app: "Editor".into(),
            title: "notes.txt — Editor".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 909_301;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "editor", "coordinate": [1, 1]});
        // Seed an Always grant for the ORIGINAL window's identity (id 909_301).
        let action_key = crate::ask::action_key(&[
            "gui",
            "left_click",
            "editor",
            "909301",
            "Editor",
            "notes.txt — Editor",
            &args_digest(&args),
        ]);
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: Vec::new(),
            always: vec![crate::ask::AlwaysGrant { thread, dir: dir.to_string(), action_key }],
        });

        // The ORIGINAL window closed; a REPLACEMENT opened with the SAME
        // app+title but a NEW id (909_302) — standing in for a relaunched
        // app / reopened document with an identical name.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 909_302,
            app: "Editor".into(),
            title: "notes.txt — Editor".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks_bg = asks.clone();
        let args_bg = args.clone();
        let handle = tokio::spawn(async move { approve(&asks_bg, thread, dir, None, "left_click", &args_bg).await });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect(
            "a REPLACED window instance (new id, same app+title) must miss the standing grant's \
             key — bound to the OLD id — and surface a fresh card",
        );

        assert!(asks.answer(card.id, crate::ask::Answer::Allow));
        let approved = handle.await.unwrap().unwrap();
        assert_eq!(
            approved,
            Some(ApprovedWindow { id: 909_302, app: "Editor".into(), title: "notes.txt — Editor".into() }),
            "the fresh card's own Allow binds the NEW window instance"
        );
    }

    /// The mirror image, for completeness: the EXACT same window instance
    /// (same id+app+title) still auto-approves via the standing grant — this
    /// round's narrowing must not re-card a window that never actually
    /// changed. (`standing_always_grant_auto_approves_when_the_window_
    /// identity_is_unchanged` above already covers this end-to-end; this is
    /// a second, differently-shaped window/query pair for variety, not a
    /// duplicate of that test's own assertions.)
    #[tokio::test]
    async fn standing_always_grant_auto_approves_the_exact_same_window_instance() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 909_401,
            app: "Notes".into(),
            title: "Untitled".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 909_401;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "notes", "coordinate": [2, 2]});
        let action_key = crate::ask::action_key(&[
            "gui",
            "left_click",
            "notes",
            "909401",
            "Notes",
            "Untitled",
            &args_digest(&args),
        ]);
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: Vec::new(),
            always: vec![crate::ask::AlwaysGrant { thread, dir: dir.to_string(), action_key }],
        });

        let approved = tokio::time::timeout(std::time::Duration::from_secs(5), approve(&asks, thread, dir, None, "left_click", &args))
            .await
            .expect("must resolve without ever needing a human answer")
            .expect("an exact id+app+title match must auto-approve");
        assert_eq!(approved, Some(ApprovedWindow { id: 909_401, app: "Notes".into(), title: "Untitled".into() }));
        assert!(asks.open().is_empty(), "an auto-approved call must never surface a card");
    }

    /// `list_windows` (no window argument at all) and `wait` (Write-classified
    /// but windowless) never bind a window: `approve` returns `Ok(None)` for
    /// both, and never even attempts a resolve for either — there is nothing
    /// for `verify_approved_target` to ever check against, and their
    /// `action_key` keeps the OLD, resolve-free four-part shape.
    ///
    /// Note: this test USED TO also cover
    /// `screenshot` here (asserting it never binds a window either) — that is
    /// no longer true: `screenshot` now resolves and binds a window exactly
    /// like a Write action does (see `approve`'s own `resolved` doc comment)
    /// — see `screenshot_binds_its_resolved_window_like_a_write_action` and
    /// `screenshot_fails_closed_when_its_window_arg_cannot_resolve` below for
    /// screenshot's OWN, now-different behavior.
    #[tokio::test]
    async fn list_windows_and_wait_never_bind_a_window_arg() {
        let asks = AskRegistry::new();
        let thread = 907_601;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        let list_windows_args = json!({"action": "list_windows"});
        let approved_lw = approve(&asks, thread, dir, None, "list_windows", &list_windows_args).await.unwrap();
        assert!(approved_lw.is_none());

        let wait_args = json!({"action": "wait", "duration_ms": 1});
        let approved_wait = approve(&asks, thread, dir, None, "wait", &wait_args).await.unwrap();
        assert!(
            approved_wait.is_none(),
            "wait is Write-classified but has no window argument to bind: {approved_wait:?}"
        );
    }

    // —— screenshot ALSO binds its resolved window ——

    /// The end-to-end property the fix exists for: a `screenshot` with a
    /// non-blank, resolvable `window` argument now binds that window's
    /// identity in `approve`'s return value, exactly like a Write action
    /// does — closing the gap where a screenshot's standing grant/card used
    /// to be scoped to the bare QUERY STRING alone (see `approve`'s own doc
    /// comment for the full rationale).
    #[tokio::test]
    async fn screenshot_binds_its_resolved_window_like_a_write_action() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 907_602,
            app: "Anything".into(),
            title: "anything window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 907_602;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        let screenshot_args = json!({"action": "screenshot", "window": "anything"});
        let approved = approve(&asks, thread, dir, None, "screenshot", &screenshot_args).await.unwrap();
        assert_eq!(
            approved,
            Some(ApprovedWindow { id: 907_602, app: "Anything".into(), title: "anything window".into() }),
            "a resolvable screenshot must bind the SAME window identity a Write action would"
        );
    }

    /// The fail-closed half: a `screenshot` whose `window` argument does NOT
    /// resolve to any visible window must reject the WHOLE call — never
    /// silently proceed with `Ok(None)` the way an unresolvable query used to
    /// before this change (when screenshot never attempted a resolve at all).
    #[tokio::test]
    async fn screenshot_fails_closed_when_its_window_arg_cannot_resolve() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        // No window matches "nonexistent-window-query" at all.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 907_603,
            app: "SomethingElse".into(),
            title: "unrelated".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 907_603;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        let screenshot_args = json!({"action": "screenshot", "window": "nonexistent-window-query"});
        let err = approve(&asks, thread, dir, None, "screenshot", &screenshot_args).await.unwrap_err();
        assert!(err.to_lowercase().contains("no visible window"), "{err}");
    }

    /// The end-to-end property `screenshot`'s NEW capture-time identity check
    /// exists for: a card is opened for
    /// `screenshot @ shifty`, binding window X's identity the instant it
    /// opens; while the human sits on that still-open card, the ORIGINAL
    /// window closes and a DIFFERENT one — same title substring, so the SAME
    /// query still matches — takes its place. The human's (now-stale) Allow
    /// must NOT let the capture proceed against the replacement: `run_action`'s
    /// own "screenshot" arm re-resolves fresh and must reject, exactly like
    /// every input arm's own `resolve_and_verify_target` gate — no pixels are
    /// ever captured of the wrong window.
    #[tokio::test]
    async fn screenshot_fails_closed_when_the_window_is_replaced_between_approval_and_capture() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 911_101,
            app: "Shifty".into(),
            title: "shifty window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        // Deliberately UNGRANTED — a real card must open, binding window X's
        // identity at the instant it does.
        let asks = AskRegistry::new();
        let thread = 911_101;
        let dir = "lead";

        let db_bg = db.clone();
        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move {
            let mut window_id_out = None;
            let mut image_out = None;
            run_action(
                &db_bg, &asks_bg, thread, dir, None, session_token_generation(thread, dir, None), "computer", "screenshot",
                &json!({"window": "shifty"}), &mut window_id_out, &mut image_out,
            )
            .await
        });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect("a screenshot with no standing grant must surface a card");

        // The window is REPLACED while the card sits open: the SAME title
        // substring still matches, but a DIFFERENT id and app — standing in
        // for the original window closing and an unrelated one taking its
        // place before the human answers.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 911_102,
            app: "Different App".into(),
            title: "shifty window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        assert!(asks.answer(card.id, crate::ask::Answer::Allow));
        let err = handle.await.unwrap().unwrap_err();
        assert!(
            err.contains("changed since this action was approved"),
            "must fail closed with the re-approve message, never silently capture the replacement: {err}"
        );
        assert!(
            computer::shot_dims_for(
                thread,
                dir,
                None,
                &computer::WindowInfo {
                    id: 911_102,
                    app: "Different App".into(),
                    title: "shifty window".into(),
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                }
            )
            .is_none(),
            "a fail-closed capture must never record shot dims for the replacement window either"
        );
    }

    /// The Always-grant twin of the test above: a standing Always grant
    /// scoped to window X's identity must MISS once the query resolves to a
    /// DIFFERENT window Y — falling through to a fresh card rather than
    /// silently auto-capturing Y under a grant the human only ever earned for
    /// X. Mirrors `standing_always_grant_misses_a_different_window_identity_
    /// and_cards` (the click-family version) for `screenshot` specifically.
    #[tokio::test]
    async fn screenshot_standing_always_grant_misses_a_replaced_window_and_cards() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 911_201,
            app: "Shifty".into(),
            title: "shifty window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 911_201;
        let dir = "lead";
        let args = json!({"action": "screenshot", "window": "shifty"});
        // Seed an Always grant for the CURRENT (soon-to-be-replaced) window's
        // identity.
        let action_key = crate::ask::action_key(&[
            "gui",
            "screenshot",
            "shifty",
            "911201",
            "Shifty",
            "shifty window",
            &args_digest(&args),
        ]);
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: Vec::new(),
            always: vec![crate::ask::AlwaysGrant { thread, dir: dir.to_string(), action_key }],
        });

        // The query now resolves to a DIFFERENT window.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 911_202,
            app: "OtherApp".into(),
            title: "an unrelated shifty window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks_bg = asks.clone();
        let args_bg = args.clone();
        let handle = tokio::spawn(async move { approve(&asks_bg, thread, dir, None, "screenshot", &args_bg).await });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect(
            "a DIFFERENT window identity must miss the screenshot's own standing grant key and \
             surface a fresh card",
        );

        assert!(asks.answer(card.id, crate::ask::Answer::Deny));
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.contains("denied"), "{err}");
    }

    /// the FINAL,
    /// SYNCHRONOUS stop/lease recheck each input arm runs INSIDE its
    /// `on_blocking` closure right before the backend call. Touches the SAME
    /// process-wide stop-latch/control-lease statics as every other lease test
    /// here, so it shares `process_state_test_lock`. No `.await` — the whole
    /// point is that it's a lock-only check runnable on a blocking thread.
    #[test]
    fn recheck_stop_and_lease_before_backend_covers_stop_lease_and_happy_path() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let thread = 906_301;
        let dir = "lead";

        // Clean baseline: clear any latch a prior test left (trip mints+returns
        // a generation and clears the lease, so clear it right back), then take
        // the lease for THIS (thread, dir).
        let base_gen = computer::trip_stop_latch();
        assert!(computer::clear_emergency_stop(base_gen));
        computer::acquire_control(thread, dir, None).unwrap();
        // Latch clear AND this exact (thread, dir) holds the lease — the only
        // combination allowed to reach the injection backend.
        assert!(recheck_stop_and_lease_before_backend(thread, dir, None, session_token_generation(thread, dir, None)).is_ok());

        // Emergency Stop landing after the last async recheck (during the final
        // resolve, or while the closure sat queued for a blocking thread) trips
        // the latch — deny with the disabled message. `trip_stop_latch` ALSO
        // clears the lease, so the stop check firing first is what this asserts.
        let stop_gen = computer::trip_stop_latch();
        let err = recheck_stop_and_lease_before_backend(thread, dir, None, session_token_generation(thread, dir, None)).unwrap_err();
        assert!(err.to_lowercase().contains("disabled"), "{err}");
        assert!(computer::clear_emergency_stop(stop_gen));

        // Latch clear, but the lease is gone (Escape cleared it / it expired in
        // that same window) — deny, and NOT with the disabled message.
        computer::clear_control();
        let err = recheck_stop_and_lease_before_backend(thread, dir, None, session_token_generation(thread, dir, None)).unwrap_err();
        assert!(!err.to_lowercase().contains("disabled"), "{err}");

        // A DIFFERENT (thread, dir) now holds the lease — deny (busy).
        computer::acquire_control(999_999, "someone-else", None).unwrap();
        let err = recheck_stop_and_lease_before_backend(thread, dir, None, session_token_generation(thread, dir, None)).unwrap_err();
        assert!(err.contains("999999") || err.contains("someone-else"), "{err}");
        computer::clear_control();
    }

    /// the final recheck compares
    /// `wt` too, so a SIBLING worker (SAME `(thread, dir)`, different worktree)
    /// holding the lease is NOT mistaken for "I still hold it". Worker wt=1's
    /// recheck must fail while sibling wt=2 holds the lease, even though thread
    /// and dir match exactly; the actual holder (wt=2) still passes — proving
    /// this is the wt discrimination, not a blanket refusal.
    #[test]
    fn recheck_before_backend_rejects_a_sibling_worker_holding_the_lease() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let thread = 906_401;
        let dir = "55";

        let base_gen = computer::trip_stop_latch();
        assert!(computer::clear_emergency_stop(base_gen));

        // Sibling worker wt=2 holds the lease.
        computer::acquire_control(thread, dir, Some(2)).unwrap();

        // Worker wt=1's final recheck must NOT pass — a busy/lease rejection,
        // not a stop (the latch is clear).
        let err = recheck_stop_and_lease_before_backend(thread, dir, Some(1), session_token_generation(thread, dir, Some(1))).unwrap_err();
        assert!(
            !err.to_lowercase().contains("disabled"),
            "must be a busy/lease rejection, not a stop: {err}"
        );

        // The actual holder (wt=2) still passes.
        assert!(recheck_stop_and_lease_before_backend(thread, dir, Some(2), session_token_generation(thread, dir, Some(2))).is_ok());
        computer::clear_control();
    }

    /// the post-queue
    /// liveness recheck validates the request's OWN `wt` — a session-only
    /// worker whose worktree a repo delete removed (direction SURVIVING) must
    /// be refused here even though it holds a freshly-acquired lease, while
    /// the direction's surviving sibling worktree still passes.
    #[tokio::test]
    async fn recheck_after_guard_rejects_a_deleted_secondary_worktree_session() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let repo_a =
            repo::add_repo_ref(&db, ws.id, "a", &tmp.path().to_string_lossy(), "main", "", true)
                .await
                .unwrap();
        let thread = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let direction = repo::create_direction(
            &db, thread.id, "task", "claude", repo_a.id, "why", "impl-only", "main",
        )
        .await
        .unwrap();
        // The SURVIVING sibling worktree (repo A's own).
        let wt_a =
            repo::record_worktree(&db, repo_a.id, direction.id, "b1", "/tmp/weft-wt-a", true, true, "")
                .await
                .unwrap();
        // Repo B's worktree was deleted — no row exists for this id, and the
        // delete fence published a session-only revocation for it.
        let wt_b_id = wt_a.id + 991;
        let dir_s = direction.id.to_string();
        let restore = snapshot_revocations(&[thread.id]);
        revoke_computer_route_session(thread.id, dir_s.clone(), wt_b_id);

        // The stale request acquired a fresh lease AFTER the delete (nothing
        // existed to clear at delete time) — exactly the reported scenario.
        computer::acquire_control(thread.id, &dir_s, Some(wt_b_id)).unwrap();
        let asks = AskRegistry::new();
        let err = recheck_after_guard(&db, &asks, thread.id, &dir_s, Some(wt_b_id), session_token_generation(thread.id, &dir_s, Some(wt_b_id)))
            .await
            .expect_err("a deleted secondary worktree's session must be refused, lease or not");
        assert!(err.contains("no longer exists"), "{err}");
        computer::clear_control();

        // The surviving sibling passes the same checkpoint with its own lease.
        computer::acquire_control(thread.id, &dir_s, Some(wt_a.id)).unwrap();
        assert!(
            recheck_after_guard(&db, &asks, thread.id, &dir_s, Some(wt_a.id), session_token_generation(thread.id, &dir_s, Some(wt_a.id))).await.is_ok(),
            "the surviving sibling worktree must not be caught by the session revocation"
        );
        computer::clear_control();
        restore_revocations(restore);
    }

    /// deleting a thread removes
    /// ITS computer-output subtree only, never a sibling thread's.
    #[test]
    fn remove_computer_output_for_thread_drops_only_that_threads_subtree() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home =
            std::env::temp_dir().join(format!("weft-computer-srv-rm-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let root = crate::paths::computer_output_root().unwrap();
        // Thread 42 (lead lane): a screenshot plus a rotated-style audit log.
        let shots_42 = root.join("42").join(crate::bus::LEAD).join("screenshots");
        std::fs::create_dir_all(&shots_42).unwrap();
        std::fs::write(shots_42.join("shot.png"), b"x").unwrap();
        std::fs::write(root.join("42").join("computer-audit.jsonl"), b"{}").unwrap();
        // Sibling thread 43 that must survive the delete untouched.
        let shots_43 = root.join("43").join(crate::bus::LEAD).join("screenshots");
        std::fs::create_dir_all(&shots_43).unwrap();
        std::fs::write(shots_43.join("shot.png"), b"y").unwrap();

        remove_computer_output_for_thread(42);

        assert!(!root.join("42").exists(), "thread 42's whole subtree is removed");
        assert!(root.join("43").exists(), "a sibling thread's subtree is untouched");
        assert!(shots_43.join("shot.png").exists(), "the sibling's screenshot survives");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// the recursive
    /// cleanup helpers must never delete THROUGH a symlinked ancestor — a
    /// pre-planted symlink at `computer/` would otherwise redirect
    /// `remove_dir_all` into `<target>/<thread>` OUTSIDE Weft-managed storage.
    /// A refused chain skips the best-effort cleanup; the symlink's target
    /// survives untouched.
    #[cfg(unix)]
    #[test]
    fn remove_computer_output_refuses_a_symlinked_computer_ancestor() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home =
            std::env::temp_dir().join(format!("weft-rm-symanc-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("weft-rm-symanc-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&weft_home).unwrap();
        // The would-be victim: real content under the symlink's TARGET.
        std::fs::create_dir_all(outside.join("42")).unwrap();
        std::fs::write(outside.join("42").join("audit.jsonl"), b"{}").unwrap();
        std::fs::create_dir_all(outside.join("7").join("10")).unwrap();
        std::fs::write(outside.join("7").join("10").join("shot.png"), b"x").unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        // `computer/` itself is a symlink pointing outside weft-managed storage.
        std::os::unix::fs::symlink(&outside, weft_home.join("computer")).unwrap();

        remove_computer_output_for_thread(42);
        remove_computer_output_for_direction(7, "10");

        assert!(
            outside.join("42").join("audit.jsonl").exists(),
            "a symlinked computer/ ancestor must refuse the thread cleanup, not delete the target"
        );
        assert!(
            outside.join("7").join("10").join("shot.png").exists(),
            "…and the direction cleanup likewise"
        );

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&weft_home);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// the post-capture
    /// cleanup removes exactly what THIS call's save recreated — the
    /// screenshots subtree plus any now-empty ancestors — while a surviving
    /// sibling direction's tree stops the ancestor pruning (non-recursive
    /// `remove_dir` fails on non-empty), and the shared `computer/` root
    /// always survives.
    #[test]
    fn remove_recreated_screenshot_output_prunes_own_chain_but_not_siblings() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home =
            std::env::temp_dir().join(format!("weft-rm-recreated-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let root = crate::paths::computer_output_root().unwrap();
        let out_dir = root.join("81").join("10").join("wt-1").join("screenshots");
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(out_dir.join("shot.png"), b"x").unwrap();
        // A surviving sibling direction under the same thread.
        let sibling = root.join("81").join("11");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("keep.txt"), b"k").unwrap();

        remove_recreated_screenshot_output(&out_dir);

        assert!(!out_dir.exists(), "the recreated screenshots subtree must be removed");
        assert!(!root.join("81").join("10").exists(), "empty ancestors are pruned");
        assert!(
            root.join("81").exists(),
            "the thread dir survives — the sibling keeps it non-empty"
        );
        assert!(sibling.join("keep.txt").exists(), "a sibling direction's tree is untouched");
        assert!(root.exists(), "the shared computer/ root always survives");

        // With no sibling left, the whole chain up to computer/ goes.
        let out2 = root.join("82").join("10").join("wt-1").join("screenshots");
        std::fs::create_dir_all(&out2).unwrap();
        remove_recreated_screenshot_output(&out2);
        assert!(!root.join("82").exists(), "a fully-empty chain prunes up to computer/");
        assert!(root.exists());

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// Same symlink doctrine for the earlier cleanup: a symlinked
    /// `computer/` ancestor refuses the removal outright (the target
    /// survives), and an `out_dir` outside `<weft_home>` is never touched.
    #[cfg(unix)]
    #[test]
    fn remove_recreated_screenshot_output_refuses_symlinked_or_foreign_paths() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home =
            std::env::temp_dir().join(format!("weft-rm-recreated-sym-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("weft-rm-recreated-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&weft_home).unwrap();
        let victim = outside.join("9").join("10").join("screenshots");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("shot.png"), b"x").unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        std::os::unix::fs::symlink(&outside, weft_home.join("computer")).unwrap();

        // Through the symlinked ancestor: refused, target intact.
        remove_recreated_screenshot_output(
            &weft_home.join("computer").join("9").join("10").join("screenshots"),
        );
        assert!(
            victim.join("shot.png").exists(),
            "a symlinked computer/ ancestor must refuse the cleanup, not delete the target"
        );

        // An out_dir not under weft_home at all: never touched.
        remove_recreated_screenshot_output(&victim);
        assert!(
            victim.join("shot.png").exists(),
            "a path with no weft_home prefix is refused outright"
        );

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&weft_home);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// a repo delete prunes ONLY
    /// the removed direction's output subtree — a sibling direction and the lead
    /// lane under the SAME surviving thread are untouched, and a non-integer dir
    /// never touches the filesystem.
    #[test]
    fn remove_computer_output_for_direction_drops_only_that_direction() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home =
            std::env::temp_dir().join(format!("weft-computer-srv-rm-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let root = crate::paths::computer_output_root().unwrap();
        let thread = "77";
        // Direction 10 (removed by a repo delete) with a worktree subtree.
        let d10 = root.join(thread).join("10").join("wt-1").join("screenshots");
        std::fs::create_dir_all(&d10).unwrap();
        std::fs::write(d10.join("shot.png"), b"x").unwrap();
        // Direction 11 and the lead lane both survive (they are not this repo's).
        let d11 = root.join(thread).join("11").join("wt-2");
        std::fs::create_dir_all(&d11).unwrap();
        std::fs::write(d11.join("shot.png"), b"y").unwrap();
        let lead = root.join(thread).join(crate::bus::LEAD);
        std::fs::create_dir_all(&lead).unwrap();

        remove_computer_output_for_direction(77, "10");
        assert!(!root.join(thread).join("10").exists(), "the removed direction's subtree is gone");
        assert!(root.join(thread).join("11").exists(), "a surviving sibling direction is untouched");
        assert!(lead.exists(), "the lead lane is untouched");

        // A non-integer dir is bounded out — never joined, never a path escape.
        remove_computer_output_for_direction(77, "../evil");
        assert!(root.join(thread).join("11").exists(), "a malformed dir touches nothing");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// deleting a SESSION-ONLY
    /// worker (a secondary repo's worktree under a SURVIVING direction) prunes
    /// exactly `computer/<thread>/<dir>/wt-<id>` — the direction's sibling
    /// worktree, its own other content, and the lead lane are untouched, and a
    /// malformed dir never touches the filesystem.
    #[test]
    fn remove_computer_output_for_worktree_drops_only_that_worktree() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home =
            std::env::temp_dir().join(format!("weft-computer-srv-rm-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let root = crate::paths::computer_output_root().unwrap();
        let thread = "78";
        // The deleted secondary worktree (wt-5) and its surviving sibling
        // (wt-6) under the SAME surviving direction.
        let doomed = root.join(thread).join("20").join("wt-5").join("screenshots");
        std::fs::create_dir_all(&doomed).unwrap();
        std::fs::write(doomed.join("shot.png"), b"x").unwrap();
        let sibling = root.join(thread).join("20").join("wt-6");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("shot.png"), b"y").unwrap();
        let lead = root.join(thread).join(crate::bus::LEAD);
        std::fs::create_dir_all(&lead).unwrap();

        remove_computer_output_for_worktree(78, "20", 5);
        assert!(
            !root.join(thread).join("20").join("wt-5").exists(),
            "the deleted worktree's subtree is gone"
        );
        assert!(
            sibling.join("shot.png").exists(),
            "the surviving sibling worktree of the SAME direction is untouched"
        );
        assert!(lead.exists(), "the lead lane is untouched");

        // A non-integer dir is bounded out — never joined, never a path escape.
        remove_computer_output_for_worktree(78, "../evil", 5);
        assert!(sibling.join("shot.png").exists(), "a malformed dir touches nothing");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// a request under a
    /// deleted thread/direction is refused — `session_is_live` is the admission
    /// gate that revokes the route once the delete cascade runs.
    #[tokio::test]
    async fn session_is_live_tracks_thread_and_direction_deletion() {
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let repo_ref =
            repo::add_repo_ref(&db, ws.id, "a", &tmp.path().to_string_lossy(), "main", "", true)
                .await
                .unwrap();
        let thread = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let direction = repo::create_direction(
            &db, thread.id, "task", "claude", repo_ref.id, "why", "impl-only", "main",
        )
        .await
        .unwrap();
        let wt = repo::record_worktree(&db, repo_ref.id, direction.id, "b1", "/tmp/weft-wt", true, true, "")
            .await
            .unwrap();
        let dir_s = direction.id.to_string();

        // Live: lead lane, worker lane, and a pinned worktree all resolve.
        assert!(session_is_live(&db, thread.id, crate::bus::LEAD, None).await);
        assert!(session_is_live(&db, thread.id, &dir_s, None).await);
        assert!(session_is_live(&db, thread.id, &dir_s, Some(wt.id)).await);
        // A worktree that isn't this direction's, and a direction under a
        // DIFFERENT thread, both fail closed.
        assert!(!session_is_live(&db, thread.id, &dir_s, Some(wt.id + 999)).await);
        assert!(!session_is_live(&db, thread.id + 999, &dir_s, None).await);
        assert!(!session_is_live(&db, thread.id, "not-a-number", None).await);

        // After the delete cascade, every lane is refused.
        repo::delete_thread_cascade(&db, thread.id).await.unwrap();
        assert!(!session_is_live(&db, thread.id, crate::bus::LEAD, None).await);
        assert!(!session_is_live(&db, thread.id, &dir_s, None).await);
        assert!(!session_is_live(&db, thread.id, &dir_s, Some(wt.id)).await);
    }

    /// The
    /// revocation map is empty until a delete (so no live session — nor any
    /// synthetic-identity test — ever pays the `session_is_live` DB check), and
    /// it distinguishes a WHOLE-thread delete from a per-DIRECTION (repo) delete
    /// so the latter can't strand a multi-repo thread's surviving directions.
    #[test]
    fn revocation_map_distinguishes_whole_thread_from_per_direction() {
        let whole = 917_001;
        let repo_thread = 917_002;
        let untouched = 917_003;
        // Leave the process-global map exactly as found.
        let restore = snapshot_revocations(&[whole, repo_thread, untouched]);

        assert!(!computer_routes_revoked(untouched), "a never-deleted thread is not revoked");
        assert!(!route_revoked_sync(untouched, "5", None), "…and no direction is sync-revoked");

        // Whole-thread revoke: coarse gate fires and EVERY dir is sync-revoked.
        revoke_computer_routes(whole);
        assert!(computer_routes_revoked(whole));
        assert!(route_revoked_sync(whole, "5", None), "a whole-thread delete revokes every worker dir");
        assert!(route_revoked_sync(whole, crate::bus::LEAD, None), "…and the lead lane");

        // Per-direction revoke: coarse gate fires (so `session_is_live` runs) but
        // ONLY the removed direction is sync-revoked — siblings and lead survive.
        revoke_computer_route_dir(repo_thread, "10".to_string());
        assert!(computer_routes_revoked(repo_thread), "coarse gate fires so session_is_live runs");
        assert!(route_revoked_sync(repo_thread, "10", None), "the removed direction is refused");
        assert!(!route_revoked_sync(repo_thread, "11", None), "a surviving sibling direction is NOT refused");
        assert!(!route_revoked_sync(repo_thread, crate::bus::LEAD, None), "the lead lane survives a repo delete");

        restore_revocations(restore);
        assert!(!computer_routes_revoked(whole), "cleanup restored the empty baseline");
        assert!(!computer_routes_revoked(repo_thread));
    }

    /// a SESSION-ONLY worker
    /// revocation — repo B's worktree deleted while the direction (owned by
    /// repo A) survives — refuses exactly `(dir, wt_b)`: the coarse gate fires
    /// (so `session_is_live` runs for every request of the thread), the doomed
    /// worktree is sync-revoked, and the direction's surviving sibling
    /// worktree, its `wt`-less resolution, and the lead lane all stay live.
    /// Composes with a dir-level revocation on the SAME thread without either
    /// clobbering the other.
    #[test]
    fn revocation_map_supports_session_only_worker_revocation() {
        let thread = 917_020;
        let restore = snapshot_revocations(&[thread]);

        revoke_computer_route_session(thread, "30".to_string(), 7);
        assert!(computer_routes_revoked(thread), "coarse gate fires so session_is_live runs");
        assert!(route_revoked_sync(thread, "30", Some(7)), "the deleted worktree's route is refused");
        assert!(
            !route_revoked_sync(thread, "30", Some(8)),
            "a surviving sibling worktree of the SAME direction is NOT refused"
        );
        assert!(
            !route_revoked_sync(thread, "30", None),
            "the direction's wt-less resolution is not dir-revoked by a session revocation"
        );
        assert!(!route_revoked_sync(thread, crate::bus::LEAD, None), "the lead lane survives");

        // A dir-level revocation merges into the same Partial entry.
        revoke_computer_route_dir(thread, "31".to_string());
        assert!(route_revoked_sync(thread, "31", None), "the dir revocation lands alongside");
        assert!(route_revoked_sync(thread, "30", Some(7)), "…without dropping the session one");

        restore_revocations(restore);
        assert!(!computer_routes_revoked(thread));
    }

    /// a failed cascade's
    /// rollback must restore EXACTLY the prior revocation state — never drop a
    /// revocation that an earlier successful delete already published.
    #[test]
    fn revocation_snapshot_restore_preserves_a_prior_revocation() {
        let thread = 917_010;
        let cleanup = snapshot_revocations(&[thread]);

        // An earlier repo delete already revoked direction "20".
        revoke_computer_route_dir(thread, "20".to_string());
        assert!(route_revoked_sync(thread, "20", None));

        // A later repo delete snapshots, revokes "21", then FAILS and restores.
        let undo = snapshot_revocations(&[thread]);
        revoke_computer_route_dir(thread, "21".to_string());
        assert!(route_revoked_sync(thread, "21", None));
        restore_revocations(undo);

        assert!(route_revoked_sync(thread, "20", None), "a pre-existing revocation must survive the rollback");
        assert!(!route_revoked_sync(thread, "21", None), "the failed op's own revocation is rolled back");

        restore_revocations(cleanup);
        assert!(!computer_routes_revoked(thread));
    }

    // —— recheck_after_guard ——

    /// One test exercises `recheck_after_guard`'s whole matrix sequentially,
    /// mirroring `computer::tests::control_lock_busy_expiry_release_and_clear`'s
    /// own reasoning: this touches the SAME process-wide control-lease
    /// static as every other `computer::acquire_control`-touching test in
    /// this binary, so splitting these scenarios across separate `#[test]`s
    /// would let `cargo test`'s default parallel threads race each other's
    /// lease state. The `has_open` scenario
    /// lives HERE too, for the exact same reason — a separate `#[tokio::test]`
    /// that also calls `computer::acquire_control` raced this one and the
    /// OTHER lease-touching tests in this file/binary under `cargo test`'s
    /// default parallelism (confirmed: it flaked in CI-style runs).
    /// `computer::process_state_test_lock` closes that — see its own doc.
    #[tokio::test]
    async fn recheck_after_guard_covers_disabled_foreign_lease_missing_lease_and_the_happy_path() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 905_001;
        let dir = "lead";

        // Disabled setting denies regardless of the lease.
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "false").await.unwrap();
        computer::clear_control();
        let err = recheck_after_guard(&db, &asks, thread, dir, None, session_token_generation(thread, dir, None)).await.unwrap_err();
        assert!(err.to_lowercase().contains("disabled"), "{err}");

        // Enabled, but nobody holds the lease at all (it expired, or was
        // cleared by an emergency stop while this call was queued) — denied,
        // not silently allowed just because the setting itself reads true.
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        computer::clear_control();
        let err = recheck_after_guard(&db, &asks, thread, dir, None, session_token_generation(thread, dir, None)).await.unwrap_err();
        assert!(!err.to_lowercase().contains("disabled"), "{err}");

        // Enabled, but a DIFFERENT (thread, dir) now holds the lease
        // (preempted while this call was queued behind the flight guard) —
        // denied.
        computer::acquire_control(999_999, "someone-else", None).unwrap();
        let err = recheck_after_guard(&db, &asks, thread, dir, None, session_token_generation(thread, dir, None)).await.unwrap_err();
        assert!(err.contains("999999") || err.contains("someone-else"), "{err}");
        computer::clear_control();

        // Enabled AND this exact (thread, dir) still holds the lease —
        // passes.
        computer::acquire_control(thread, dir, None).unwrap();
        assert!(recheck_after_guard(&db, &asks, thread, dir, None, session_token_generation(thread, dir, None)).await.is_ok());

        // a brand-new, unrelated ask opening
        // for this EXACT (thread, dir) — simulating one that opened WHILE this
        // call sat queued on `input_flight_guard` — must now deny with the
        // SAME `SuspendedPendingAsk` text `check_suspended`'s own up-front
        // check returns, even though the lease and the enabled setting are
        // both still fine (still held from the scenario right above). A
        // DIFFERENT (thread, dir)'s own open ask must not leak into this one.
        let (other_id, _rx) =
            asks.request(thread, "some-other-dir", "tool", "summary", "detail", crate::ask::RiskLevel::Unknown, "[]");
        assert!(
            recheck_after_guard(&db, &asks, thread, dir, None, session_token_generation(thread, dir, None)).await.is_ok(),
            "a DIFFERENT (thread, dir)'s open ask must not affect this one"
        );
        assert!(asks.answer(other_id, crate::ask::Answer::Deny));

        let (id, _rx) = asks.request(thread, dir, "tool", "summary", "detail", crate::ask::RiskLevel::Unknown, "[]");
        let err = recheck_after_guard(&db, &asks, thread, dir, None, session_token_generation(thread, dir, None)).await.unwrap_err();
        assert!(err.contains("permission card"), "{err}");

        // Once answered, the recheck passes again.
        assert!(asks.answer(id, crate::ask::Answer::Deny));
        assert!(recheck_after_guard(&db, &asks, thread, dir, None, session_token_generation(thread, dir, None)).await.is_ok());

        computer::clear_control();
    }

    // —— re-check the kill switch AFTER approval ——

    /// The human hits Stop (disabling the setting) WHILE a card is still
    /// open; a stale Allow answers that SAME card anyway (e.g. a click that
    /// raced the Stop). The re-check inside `run_action`, right after
    /// `approve` returns, must still deny — and never reach dispatch.
    ///
    /// Note: `approve` itself resolves the
    /// target window authoritatively as soon as the human's Allow lands — this needs a resolvable "notes" window
    /// (`shared_mock`, under `process_state_test_lock`) so that resolve
    /// itself succeeds and control actually reaches the kill-switch re-check
    /// this test exists to exercise, rather than failing earlier on an
    /// unrelated `WindowNotFound`/`Unsupported` from whatever backend state
    /// another concurrently-running test happens to have left in place.
    #[tokio::test]
    async fn disabling_the_setting_while_a_card_is_open_still_denies_a_later_allow() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 901_101,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        let asks = AskRegistry::new();
        let asks_bg = asks.clone();
        let db_bg = db.clone();
        let thread = 901_101;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "notes", "coordinate": [1, 1]});

        let handle = tokio::spawn(async move {
            let mut window_id_out = None;
            let mut image_out = None;
            run_action(
                &db_bg, &asks_bg, thread, dir, None, session_token_generation(thread, dir, None), "computer", "left_click", &args,
                &mut window_id_out, &mut image_out,
            )
            .await
        });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect("a Needs-you card must appear for this call");

        // The human hits Stop: disable the setting WHILE the card is open.
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "false")
            .await
            .unwrap();

        // ...then a stale Allow answers the SAME card anyway.
        assert!(asks.answer(card.id, crate::ask::Answer::Allow));

        let err = handle.await.unwrap().unwrap_err();
        assert!(
            err.to_lowercase().contains("disabled"),
            "the re-check must deny with the disabled message, not proceed to dispatch: {err}"
        );
    }

    // —— coordinate mapping by SAVED screenshot geometry ——

    /// The fail-closed half, end-to-end through `run_action`'s own "left_click"
    /// arm (Full-granted, so `approve` decides silently): a window that
    /// resolves fine but was NEVER screenshotted for this exact (thread, dir)
    /// has no screenshot-space coordinate system to map against at all —
    /// the click must be rejected, never silently fall back to mapping
    /// against the window's CURRENT size (the exact bug this change closes).
    #[tokio::test]
    async fn left_click_fails_closed_when_no_screenshot_is_on_file_for_the_window() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 912_001,
            app: "Fresh".into(),
            title: "never screenshotted".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 912_001;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });
        // Deliberately NO `computer::record_shot_dims` call for this
        // (thread, dir, 912_001) — this window has never been screenshotted.

        // Clear the global input throttle window so this call isn't itself
        // rejected as rate-limited (which would mask the property under
        // test) — mirrors every other real-input test in this module.
        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, session_token_generation(thread, dir, None), "computer", "left_click",
            &json!({"window": "fresh", "coordinate": [1, 1]}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(err.to_lowercase().contains("screenshot"), "{err}");
        // the missing
        // prerequisite is now PREFLIGHTED before activation, so the doomed
        // call must not have raised/focused the target window either — not
        // just skipped the click.
        assert!(
            mock.actions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .all(|a| !a.starts_with("click") && !a.starts_with("activate")),
            "a fail-closed coordinate mapping must reach neither the click NOR the activation"
        );
        computer::clear_control();
    }

    /// The end-to-end property this whole round exists for (the review's own
    /// example: a 2000px window screenshotted at a downscaled 1280px, then
    /// resized DOWN to 1000px before the next click): a coordinate read off
    /// the SAVED screenshot must map to the SAME proportional position on the
    /// window's CURRENT rectangle — never a position derived from treating
    /// the screenshot as if it were sized to match the window's PRESENT
    /// dimensions (`map_to_physical`'s old bug this change replaces).
    #[tokio::test]
    async fn left_click_maps_the_screenshot_coordinate_proportionally_after_a_resize() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        // The window's CURRENT size — already resized DOWN from whatever it
        // was at screenshot time.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 912_101,
            app: "Resizable".into(),
            title: "resizable window".into(),
            x: 0,
            y: 0,
            width: 1000,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 912_101;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });
        // The screenshot this agent is reading coordinates off of was saved
        // at 1280x800 — BEFORE the window resized down to 1000x600 above.
        // geometry is now ALSO recorded here
        // (the window's own pre-resize rect), but `shot_dims_for` does not
        // gate on it — see that function's own doc for why this exact
        // resize-tolerance property (`left_click_maps_the_screenshot_
        // coordinate_proportionally_after_a_resize`, THIS test) is one of
        // the two reasons it doesn't.
        computer::record_shot_dims(
            thread,
            dir,
            None,
            912_101,
            1280,
            800,
            &computer::WindowInfo {
                id: 912_101,
                app: "Resizable".into(),
                title: "resizable window".into(),
                x: 0,
                y: 0,
                width: 1280,
                height: 800,
            },
        );

        // Clear the global input throttle window so this call isn't itself
        // rejected as rate-limited.
        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        let mut window_id_out = None;
        let mut image_out = None;
        // (640, 400) is the exact midpoint of the 1280x800 screenshot.
        let result = run_action(
            &db, &asks, thread, dir, None, session_token_generation(thread, dir, None), "computer", "left_click",
            &json!({"window": "resizable", "coordinate": [640, 400]}),
            &mut window_id_out, &mut image_out,
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
        let actions = mock.actions.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            actions.iter().any(|a| a == "click 500,300 Left x1"),
            "must land at the CURRENT window's own midpoint (500, 300 of 1000x600), not a position \
             derived from the window's now-stale 1280x800 screenshot-time size: {actions:?}"
        );
        drop(actions);
        computer::clear_control();
    }

    // —— screenshot capture/encode concurrency cap ——

    /// The mechanism itself: `SCREENSHOT_CONCURRENCY` permits total, so
    /// draining that many exhausts the semaphore and a further acquire must
    /// queue (not resolve) until one of the held permits is released — the
    /// exact property `run_action`'s `screenshot` arm relies on to keep at
    /// most `SCREENSHOT_CONCURRENCY` captures resident at once. Tested
    /// directly against the semaphore rather than through a full end-to-end
    /// `run_action` call: `shared_mock`'s `MockBackend.image` is fixed at
    /// construction (`None`, per its own doc — no test in this module can
    /// configure a delayed/blocking capture), so a real concurrent-capture
    /// race can't be reproduced through the mock; the semaphore this arm
    /// acquires BEFORE ever touching the backend is the actual unit
    /// enforcing the cap, and is what this test exercises.
    #[tokio::test]
    async fn screenshot_semaphore_caps_concurrent_capture_at_the_configured_limit() {
        let mut held = Vec::new();
        for _ in 0..SCREENSHOT_CONCURRENCY {
            held.push(
                screenshot_semaphore()
                    .acquire()
                    .await
                    .expect("semaphore is never closed"),
            );
        }
        assert_eq!(
            screenshot_semaphore().available_permits(),
            0,
            "every permit must be held once SCREENSHOT_CONCURRENCY captures are in flight"
        );

        // A queued (N+1)th caller must not proceed while every permit is held.
        let queued = tokio::spawn(async {
            let _permit = screenshot_semaphore()
                .acquire()
                .await
                .expect("semaphore is never closed");
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !queued.is_finished(),
            "a capture beyond the concurrency cap must queue, never run alongside the others already in flight"
        );

        // Releasing ONE held permit lets the queued caller through.
        held.pop();
        queued
            .await
            .expect("the queued capture must complete once a permit frees up");
    }

    /// the identity re-verification the
    /// capture semaphore's own queueing reopened. The ORIGINAL window can close and a
    /// same-query REPLACEMENT take its place while a call sits queued on the
    /// capture semaphore — `approve` and the arm's own FIRST
    /// `resolve_and_verify_target` only ever saw the ORIGINAL window. A Full
    /// grant (not a fresh card) isolates this from the
    /// pre-approval resolution gap. Without the post-queue re-check,
    /// `screenshot_window`'s own internal re-resolve would silently capture
    /// the REPLACEMENT the instant a permit frees up.
    ///
    /// `approve`'s OWN resolve holds a semaphore permit too, so a
    /// drained semaphore parks the call at APPROVE — before any
    /// resolution — and a wall-clock "swap while queued" would land before
    /// the FIRST resolve (where capturing the replacement would be CORRECT:
    /// authorization would have bound it). The swap is therefore sequenced
    /// by CALL INDEX instead (`windows_sequence`): resolve #1 (approve) and
    /// #2 (first verify) see the original, resolve #3 (the post-queue
    /// re-verify this test exists for) sees the replacement — deterministic
    /// wherever the queueing happens.
    #[tokio::test]
    async fn screenshot_re_verifies_after_the_capture_semaphore_queue_before_capturing() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let original = computer::WindowInfo {
            id: 913_301,
            app: "Queued".into(),
            title: "queued window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        };
        let replacement = computer::WindowInfo {
            id: 913_302,
            app: "Different App".into(),
            title: "queued window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        };
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(vec![replacement.clone()]);
        *mock.windows_sequence.lock().unwrap_or_else(|e| e.into_inner()) =
            std::collections::VecDeque::from(vec![
                vec![original.clone()],
                vec![original.clone()],
            ]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 913_301;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        // Drain every capture permit so this call's own acquire must queue.
        let mut held = Vec::new();
        for _ in 0..SCREENSHOT_CONCURRENCY {
            held.push(screenshot_semaphore().acquire().await.expect("semaphore is never closed"));
        }

        let db_bg = db.clone();
        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move {
            let mut window_id_out = None;
            let mut image_out = None;
            run_action(
                &db_bg, &asks_bg, thread, dir, None, session_token_generation(thread, dir, None), "computer", "screenshot",
                &json!({"window": "queued"}), &mut window_id_out, &mut image_out,
            )
            .await
        });

        // The call queues on the drained semaphore (at `approve`'s
        // own authorization-time resolve, the FIRST permit acquisition on the
        // path). The window "swap" itself needs no timing at all — the mock's
        // `windows_sequence` (seeded above) hands the original to resolves #1
        // and #2 and the replacement to resolve #3, wherever the queueing
        // happened.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(!handle.is_finished(), "the call must still be queued on the drained capture semaphore");

        // Free exactly one permit so the queued call proceeds.
        held.pop();

        let err = handle.await.unwrap().unwrap_err();
        assert!(
            err.contains("changed since this action was approved"),
            "must fail closed with the re-approve message, never silently capture the replacement: {err}"
        );
        assert!(
            computer::shot_dims_for(
                thread,
                dir,
                None,
                &computer::WindowInfo {
                    id: 913_302,
                    app: "Different App".into(),
                    title: "queued window".into(),
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                }
            )
            .is_none(),
            "a fail-closed capture must never record shot dims for the replacement window either"
        );

        drop(held);
    }

    // —— list_windows shares the observe semaphore ——

    /// `list_windows` now
    /// acquires the SAME `screenshot_semaphore` a `screenshot` capture does —
    /// closing the "no cap at all on an already-authorized session's
    /// concurrent enumeration" gap this change exists for. `MockBackend` (see
    /// its own doc comment) has no delay-injection hook for `list_windows`
    /// itself, so a genuine concurrent race can't be forced through
    /// `run_action` the way `screenshot_re_verifies_after_the_capture_
    /// semaphore_queue_before_capturing` forces one for capture (that test's
    /// own doc explains the same limitation for `MockBackend.image`) — but
    /// draining the semaphore's permits directly, exactly like
    /// `screenshot_semaphore_caps_concurrent_capture_at_the_configured_limit`
    /// does, reproduces the SAME observable effect without needing a new mock
    /// hook: a `list_windows` call must queue behind those drained permits
    /// rather than run unbounded, and complete successfully once one frees
    /// up. Driven through the full `run_action` dispatch (not the bare
    /// semaphore), so this also covers the "the observe path still returns
    /// the right data through `on_blocking`" regression for this arm.
    #[tokio::test]
    async fn list_windows_queues_behind_the_shared_observe_semaphore_then_succeeds() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 916_001,
            app: "Listed".into(),
            title: "listed window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 916_001;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        // Drain every permit the shared observe semaphore has — standing in
        // for `SCREENSHOT_CONCURRENCY` other observe calls already in
        // flight, exactly like the capture-side semaphore test does.
        let mut held = Vec::new();
        for _ in 0..SCREENSHOT_CONCURRENCY {
            held.push(screenshot_semaphore().acquire().await.expect("semaphore is never closed"));
        }

        let db_bg = db.clone();
        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move {
            let mut window_id_out = None;
            let mut image_out = None;
            run_action(
                &db_bg, &asks_bg, thread, dir, None, session_token_generation(thread, dir, None), "computer", "list_windows",
                &json!({}), &mut window_id_out, &mut image_out,
            )
            .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            !handle.is_finished(),
            "list_windows must queue behind the shared observe semaphore like screenshot does, \
             never run unbounded against an already-authorized grant"
        );

        // Free exactly one permit so the queued call proceeds.
        held.pop();

        let result = handle.await.unwrap();
        assert!(result.is_ok(), "{result:?}");
        // `computer::WindowInfo` only derives `Serialize` (this is the ONE
        // place it's ever sent back out as JSON — see this arm's own
        // dispatch), so assert against the raw `Value` rather than round-
        // tripping through a `Deserialize` this module has no other use for.
        let windows: Value = serde_json::from_str(&result.unwrap()).unwrap();
        let windows = windows.as_array().expect("list_windows returns a JSON array");
        assert_eq!(windows.len(), 1, "{windows:?}");
        assert_eq!(windows[0]["id"], json!(916_001));

        drop(held);
    }

    // —— throttle checked BEFORE the control lease
    // is (re)acquired ——

    /// The end-to-end property this fix exists for: a same-session input
    /// call faster than the global throttle window must be rejected WITHOUT
    /// renewing (extending) the control lease's own expiry. Before this
    /// round, `acquire_and_throttle` renewed the lease FIRST and only THEN
    /// checked the throttle — a loop of calls faster than the throttle
    /// window, each individually rejected, still pushed `expires_at_ms`
    /// forward on every single attempt, keeping the control lease (and the
    /// "an agent is controlling the desktop" banner/OS-level Escape shortcut
    /// it keeps registered) alive indefinitely even though no input actually
    /// ever got through.
    #[test]
    fn acquire_and_throttle_rejects_a_rate_limited_repeat_without_renewing_the_control_lease() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        // Consume + reset the throttle window far enough in the past that
        // the FIRST call below is not itself rejected as rate-limited.
        let _ = computer::throttle_input();
        std::thread::sleep(std::time::Duration::from_millis(600));

        // First call: nobody holds the lease and the throttle window has
        // already elapsed — both checks pass, taking a fresh lease.
        acquire_and_throttle(931_001, "90", None).expect("the first call must succeed");
        let after_first = computer::control_state().expect("the lease must be held after the first call");

        // A SECOND call for the SAME (thread, dir), immediately after — well
        // inside the throttle window — must be rejected...
        let err = acquire_and_throttle(931_001, "90", None)
            .expect_err("a call inside the throttle window must be rejected");
        assert!(err.to_lowercase().contains("rate-limited"), "{err}");

        // ...and the lease's own expiry must be UNCHANGED by that rejected
        // call — proving `acquire_control` was never reached for it.
        let after_second = computer::control_state()
            .expect("the lease must still be held — unaffected by the rejected call");
        assert_eq!(
            after_first.expires_at_ms, after_second.expires_at_ms,
            "a rate-limited call must not renew (extend) the control lease's expiry"
        );

        computer::clear_control();
    }

    // —— args_digest / Always-grant action_key ——

    #[test]
    fn args_digest_is_order_independent_and_stable() {
        let a = json!({"action": "left_click", "window": "notes", "coordinate": [10, 20]});
        let b = json!({"window": "notes", "coordinate": [10, 20], "action": "left_click"});
        assert_eq!(args_digest(&a), args_digest(&b));
    }

    #[test]
    fn args_digest_changes_when_typed_text_changes() {
        let a = json!({"action": "type", "window": "notes", "text": "hello"});
        let b = json!({"action": "type", "window": "notes", "text": "goodbye"});
        assert_ne!(args_digest(&a), args_digest(&b));
    }

    #[test]
    fn args_digest_never_contains_the_raw_text_and_is_a_sha256_hex_string() {
        let args = json!({"action": "type", "window": "notes", "text": "hunter2"});
        let digest = args_digest(&args);
        assert!(!digest.contains("hunter2"), "the digest must never carry the raw text: {digest}");
        assert_eq!(digest.len(), 64, "sha256 hex digest is 64 chars: {digest}");
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()), "{digest}");
    }

    #[test]
    fn args_digest_ignores_action_and_window_and_absent_keys() {
        // `action`/`window` already have their OWN slots in the action_key
        // tuple (`["gui", action, window, digest]`) — the digest itself only
        // covers `DIGEST_PARAM_KEYS`, so two calls differing ONLY in
        // action/window still produce the same digest.
        let a = json!({"action": "left_click", "window": "notes", "coordinate": [1, 2]});
        let b = json!({"action": "right_click", "window": "other", "coordinate": [1, 2]});
        assert_eq!(args_digest(&a), args_digest(&b));
    }

    /// The end-to-end property the earlier P2 §2 fix exists for: the SAME
    /// action+window+params produce the SAME `action_key` (so an Always
    /// grant is reused); a CHANGED consequential param (here, `text`) mints a
    /// DIFFERENT key (so it must re-card); and the key itself never contains
    /// the raw typed text.
    #[test]
    fn action_key_is_stable_for_repeats_and_changes_with_text_without_leaking_it() {
        let base = json!({"action": "type", "window": "notes", "text": "hello"});
        let same_different_order = json!({"window": "notes", "text": "hello", "action": "type"});
        let changed = json!({"action": "type", "window": "notes", "text": "goodbye"});

        let key1 = crate::ask::action_key(&["gui", "type", "notes", &args_digest(&base)]);
        let key2 = crate::ask::action_key(&["gui", "type", "notes", &args_digest(&same_different_order)]);
        let key3 = crate::ask::action_key(&["gui", "type", "notes", &args_digest(&changed)]);

        assert_eq!(key1, key2, "the SAME action+window+params must reuse the SAME action_key");
        assert_ne!(key1, key3, "changed text must mint a DIFFERENT action_key");
        assert!(!key1.contains("hello"), "the action_key must never contain the raw typed text: {key1}");
        assert!(!key3.contains("goodbye"), "{key3}");
    }

    // —— refuse_symlinks ——

    #[test]
    fn refuse_symlinks_accepts_an_ordinary_directory() {
        let base = std::env::temp_dir().join(format!("weft-refuse-sym-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(".weft")).unwrap();

        let out = refuse_symlinks(&base, &[".weft", "screenshots"]).unwrap();
        assert_eq!(out, base.join(".weft").join("screenshots"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn refuse_symlinks_rejects_a_symlinked_weft_directory() {
        let base = std::env::temp_dir().join(format!("weft-refuse-sym-dir-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("weft-refuse-sym-dir-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, base.join(".weft")).unwrap();

        let err = refuse_symlinks(&base, &[".weft", "screenshots"]).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        assert!(
            !outside.join("screenshots").exists(),
            "must never write through the symlinked .weft directory"
        );

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn refuse_symlinks_rejects_a_symlinked_leaf_file() {
        let base = std::env::temp_dir().join(format!("weft-refuse-sym-leaf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(".weft")).unwrap();
        let secret = std::env::temp_dir().join(format!("weft-refuse-sym-secret-{}", std::process::id()));
        std::fs::write(&secret, b"outside content").unwrap();
        std::os::unix::fs::symlink(&secret, base.join(".weft").join("computer-audit.jsonl")).unwrap();

        let err = refuse_symlinks(&base, &[".weft", "computer-audit.jsonl"]).unwrap_err();
        assert!(err.contains("symlink"), "{err}");

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_file(&secret);
    }

    // —— refuse_symlinked_output
    // walks EVERY ancestor from weft_home down, not just the appended leaf ——

    /// The ordinary case: an all-real ancestor chain resolves to
    /// `<weft_home>/computer/<thread>/<dir>/wt-<id>/<leaf>`, unchanged from what
    /// the old leaf-only `refuse_symlinks` produced.
    #[test]
    fn refuse_symlinked_output_accepts_an_ordinary_ancestor_chain() {
        let _g = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home =
            std::env::temp_dir().join(format!("weft-symanc-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("WEFT_HOME", &home);
        let root = home.join("computer").join("5").join("10").join("wt-3");
        std::fs::create_dir_all(&root).unwrap();

        let out = refuse_symlinked_output(&root, "screenshots").unwrap();
        assert_eq!(out, root.join("screenshots"));

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A symlink at the `computer/` ancestor — the FIRST segment under
    /// weft_home, and exactly the one `computer_output_root`'s own
    /// `create_dir_all` would follow — is refused, where the old leaf-only
    /// check would have written straight through it.
    #[cfg(unix)]
    #[test]
    fn refuse_symlinked_output_rejects_a_symlinked_computer_ancestor() {
        let _g = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home =
            std::env::temp_dir().join(format!("weft-symanc-comp-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("weft-symanc-comp-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::env::set_var("WEFT_HOME", &home);
        // `computer` itself is a symlink pointing outside weft-managed storage.
        std::os::unix::fs::symlink(&outside, home.join("computer")).unwrap();
        let root = home.join("computer").join("5").join("10").join("wt-3");

        let err = refuse_symlinked_output(&root, "screenshots").unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        assert!(
            !outside.join("5").join("10").join("wt-3").join("screenshots").exists(),
            "must never resolve a write through the symlinked computer ancestor"
        );

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// A symlink at the deepest `wt-<id>/` ancestor (a per-worktree directory
    /// [`session_root`] resolves) is likewise refused — the ancestor walk
    /// covers the whole chain, not only its ends.
    #[cfg(unix)]
    #[test]
    fn refuse_symlinked_output_rejects_a_symlinked_worktree_ancestor() {
        let _g = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home =
            std::env::temp_dir().join(format!("weft-symanc-wt-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("weft-symanc-wt-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        let parent = home.join("computer").join("5").join("10");
        std::fs::create_dir_all(&parent).unwrap();
        std::env::set_var("WEFT_HOME", &home);
        std::os::unix::fs::symlink(&outside, parent.join("wt-3")).unwrap();
        let root = parent.join("wt-3");

        let err = refuse_symlinked_output(&root, "computer-audit.jsonl").unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        assert!(
            !outside.join("computer-audit.jsonl").exists(),
            "must never resolve an audit write through the symlinked worktree ancestor"
        );

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&outside);
    }

    // —— O_NOFOLLOW closes the leaf open TOCTOU ——

    /// Proves the NEW defense in isolation from `refuse_symlinks`'s own,
    /// EARLIER check: the leaf path is ALREADY a symlink to an outside file
    /// by the time this open call runs (standing in for a symlink planted in
    /// the TOCTOU window between `refuse_symlinks`'s check and the real
    /// `open` — a window `refuse_symlinks` alone cannot close, since it only
    /// ever runs once, before that call). Calling
    /// [`open_audit_file_for_append`] DIRECTLY (bypassing `audit_log_path`/
    /// `refuse_symlinks` entirely) isolates that this specific open call
    /// refuses the symlink on its own — without `O_NOFOLLOW`, this exact
    /// call would silently succeed and write into `outside`.
    #[cfg(unix)]
    #[tokio::test]
    async fn open_audit_file_for_append_refuses_a_symlinked_leaf() {
        let base = std::env::temp_dir().join(format!("weft-audit-nofollow-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("weft-audit-nofollow-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_file(&outside);
        std::fs::create_dir_all(&base).unwrap();
        let leaf = base.join("computer-audit.jsonl");
        std::os::unix::fs::symlink(&outside, &leaf).unwrap();

        let result = open_audit_file_for_append(&leaf).await;
        assert!(result.is_err(), "O_NOFOLLOW must refuse to open a symlinked leaf");
        assert!(
            !outside.exists(),
            "a refused open must never create/write the symlink's target"
        );

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_file(&outside);
    }

    /// The ordinary, non-symlinked case is unaffected: `O_NOFOLLOW` only
    /// rejects a symlinked LAST component, never a plain file.
    #[cfg(unix)]
    #[tokio::test]
    async fn open_audit_file_for_append_succeeds_for_an_ordinary_file() {
        let base = std::env::temp_dir().join(format!("weft-audit-nofollow-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let leaf = base.join("computer-audit.jsonl");

        assert!(open_audit_file_for_append(&leaf).await.is_ok());
        assert!(leaf.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// a freshly-created audit file
    /// must be owner-only `0600`, never the umask-`022`-survivable `0644` a
    /// bare `open(2)` with no explicit mode would leave it at. Checked via
    /// `symlink_metadata` (not `metadata`) purely out of habit-consistency
    /// with this module's other symlink-aware checks — the leaf here is an
    /// ordinary file either way.
    #[cfg(unix)]
    #[tokio::test]
    async fn open_audit_file_for_append_creates_an_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("weft-audit-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let leaf = base.join("computer-audit.jsonl");

        let file = open_audit_file_for_append(&leaf).await.unwrap();
        drop(file);
        let mode = std::fs::symlink_metadata(&leaf).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "a fresh computer-audit.jsonl must be owner-only (0600), not the umask-survivable default"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A pre-existing audit file with a PERMISSIVE mode (the `open(2)` mode
    /// argument only applies on creation, so an old lenient file would keep
    /// receiving appends) must be set aside to a `.insecure` sibling —
    /// preserving its already-exposed bytes for inspection, never appending
    /// through it — and the path re-created owner-only. A chmod-in-place
    /// would not do: an already-open foreign read fd survives a chmod.
    #[cfg(unix)]
    #[tokio::test]
    async fn open_audit_file_for_append_sets_aside_a_permissive_preexisting_file() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("weft-audit-aside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let leaf = base.join("computer-audit.jsonl");
        std::fs::write(&leaf, b"already-exposed-lines").unwrap();
        std::fs::set_permissions(&leaf, std::fs::Permissions::from_mode(0o644)).unwrap();

        let file = open_audit_file_for_append(&leaf).await.unwrap();
        drop(file);

        let mode = std::fs::symlink_metadata(&leaf).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the re-created live file must be owner-only");
        assert_eq!(
            std::fs::read(&leaf).unwrap(),
            b"",
            "the live path must be a FRESH file, not the permissive original"
        );
        let aside = base.join("computer-audit.jsonl.insecure");
        assert_eq!(
            std::fs::read(&aside).unwrap(),
            b"already-exposed-lines",
            "the permissive original's bytes must be preserved on the sidecar"
        );

        // An already-owner-only file keeps receiving appends in place — no
        // churn to the sidecar on every open.
        let file = open_audit_file_for_append(&leaf).await.unwrap();
        drop(file);
        assert_eq!(
            std::fs::read(&aside).unwrap(),
            b"already-exposed-lines",
            "a compliant live file must never be rotated aside"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // —— bounded audit-log rotation ——

    /// The core property: a file already AT (or over) the threshold gets
    /// renamed to `<path>.1`, and the original path is free again for a
    /// fresh file (`append_audit`'s subsequent open recreates it).
    #[test]
    fn rotate_audit_at_size_renames_an_over_limit_file_to_dot_one() {
        let base = std::env::temp_dir().join(format!("weft-audit-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("computer-audit.jsonl");
        std::fs::write(&path, b"0123456789").unwrap(); // 10 bytes

        rotate_audit_at_size(&path, 10);

        assert!(!path.exists(), "the over-limit file must be rotated away from its original path");
        let rotated = base.join("computer-audit.jsonl.1");
        assert_eq!(std::fs::read(&rotated).unwrap(), b"0123456789");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A file already under the threshold is left completely alone.
    #[test]
    fn rotate_audit_at_size_is_a_no_op_below_the_threshold() {
        let base = std::env::temp_dir().join(format!("weft-audit-rotate-noop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("computer-audit.jsonl");
        std::fs::write(&path, b"tiny").unwrap();

        rotate_audit_at_size(&path, 1_000);

        assert!(path.exists(), "an under-limit file must never be rotated");
        assert!(!base.join("computer-audit.jsonl.1").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A pre-existing `.1` is overwritten by the new rotation, not appended
    /// to or left as a stray extra file — total on-disk usage stays bounded
    /// to roughly `2 * max_bytes`.
    #[test]
    fn rotate_audit_at_size_overwrites_an_existing_dot_one() {
        let base = std::env::temp_dir().join(format!("weft-audit-rotate-overwrite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("computer-audit.jsonl");
        let rotated = base.join("computer-audit.jsonl.1");
        std::fs::write(&rotated, b"stale generation").unwrap();
        std::fs::write(&path, b"0123456789").unwrap();

        rotate_audit_at_size(&path, 10);

        assert_eq!(
            std::fs::read(&rotated).unwrap(),
            b"0123456789",
            "the new rotation must overwrite the stale .1, not sit alongside it"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Missing/nonexistent path: best-effort no-op, never panics.
    #[test]
    fn rotate_audit_at_size_is_a_no_op_when_the_file_does_not_exist() {
        let base = std::env::temp_dir().join(format!("weft-audit-rotate-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("computer-audit.jsonl");

        rotate_audit_at_size(&path, 0);
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Symlink safety: a symlinked audit-log path must never be rotated
    /// (renamed) — `rotate_audit_at_size` refuses it outright rather than
    /// renaming a symlink (which would relocate the LINK, not its target,
    /// but is refused anyway per this function's own doc: `path` no longer
    /// being a REGULAR file at all is reason enough to skip).
    #[cfg(unix)]
    #[test]
    fn rotate_audit_at_size_refuses_a_symlinked_audit_path() {
        let base = std::env::temp_dir().join(format!("weft-audit-rotate-symlink-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("weft-audit-rotate-symlink-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_file(&outside);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(&outside, b"0123456789").unwrap();
        let path = base.join("computer-audit.jsonl");
        std::os::unix::fs::symlink(&outside, &path).unwrap();

        rotate_audit_at_size(&path, 10);

        assert!(path.exists(), "the symlink itself must be left in place");
        assert!(
            !base.join("computer-audit.jsonl.1").exists(),
            "a symlinked source must never be rotated"
        );
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"0123456789",
            "the symlink's target must be untouched"
        );

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_file(&outside);
    }

    /// End-to-end through `append_audit` itself (not just the pure rotation
    /// helper): a pre-existing over-limit audit file rotates to `.1` and the
    /// NEW line lands in a fresh, small file at the original path — proving
    /// the wiring inside `append_audit`, not just `rotate_audit_at_size` in
    /// isolation. Uses `rotate_audit_at_size` directly with a tiny threshold
    /// (rather than writing multiple real megabytes) to stand in for what
    /// `append_audit` itself calls unconditionally before every open.
    #[tokio::test]
    async fn append_audit_rotates_an_over_limit_file_before_writing_the_new_line() {
        let base = std::env::temp_dir().join(format!("weft-audit-rotate-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("computer-audit.jsonl");
        std::fs::write(&path, b"old-line-1\nold-line-2\n").unwrap();

        rotate_audit_at_size(&path, 5); // well under the real file's size

        let rotated = base.join("computer-audit.jsonl.1");
        assert_eq!(std::fs::read_to_string(&rotated).unwrap(), "old-line-1\nold-line-2\n");

        let mut file = open_audit_file_for_append(&path).await.unwrap();
        use tokio::io::AsyncWriteExt;
        file.write_all(b"new-line\n").await.unwrap();
        // `tokio::fs::File::write_all` only QUEUES the write onto its own
        // background blocking task — an explicit `flush` (which `append_audit`
        // itself does not call, a separate, pre-existing characteristic this
        // test doesn't change) is what this test needs to assert the file's
        // on-disk content deterministically.
        file.flush().await.unwrap();
        drop(file);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "new-line\n",
            "the new file at the original path must start fresh, not append to the rotated-away content"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// the concurrent-writer race `audit_write_lock`
    /// closes, exercised through genuine concurrency (a multi-threaded
    /// runtime, so the check/rotate/open/write sequence really can interleave
    /// across OS threads without the lock) rather than sequential calls.
    /// Pre-seeds a REAL over-limit file (`MAX_AUDIT_BYTES` + a margin) so the
    /// FIRST writer to actually acquire the lock must rotate it — then fires
    /// many concurrent appends. Without the lock, two callers could each read
    /// the pre-rotation size, both decide to rotate, and race each other's
    /// `rename`/open — losing lines or clobbering the `.1` rotation. With it,
    /// exactly one rotation happens, every line lands, and the live file
    /// never grows anywhere near the cap again for this many short lines.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_appends_serialize_through_rotation_without_losing_or_corrupting_lines() {
        let base = std::env::temp_dir().join(format!("weft-audit-concurrent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("computer-audit.jsonl");
        std::fs::write(&path, vec![b'x'; (MAX_AUDIT_BYTES + 1024) as usize]).unwrap();

        const N: usize = 25;
        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let p = path.clone();
            handles.push(tokio::spawn(async move {
                write_audit_line_locked(&p, &format!("line-{i}\n")).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let rotated = base.join("computer-audit.jsonl.1");
        assert!(rotated.exists(), "the pre-seeded over-limit original must have been rotated exactly once");
        assert!(
            std::fs::metadata(&rotated).unwrap().len() >= MAX_AUDIT_BYTES,
            "the rotated-away file must be the ORIGINAL over-limit content, never a torn/mixed one"
        );

        let live = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            live.lines().count(),
            N,
            "every concurrent append must land — none lost to an interleaved rotate/open race: {live:?}"
        );
        assert!(
            (live.len() as u64) < MAX_AUDIT_BYTES / 100,
            "the post-rotation file must stay small — no SECOND rotation was needed for {N} short \
             lines, proving no writer raced ahead of the lock to see a stale over-limit size: \
             {} bytes",
            live.len()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// End-to-end through the async path a real worker's `screenshot`/audit
    /// call takes (`session_root` → `screenshot_out_dir`/`audit_log_path`).
    /// The worker lane's output lives OFF the
    /// worktree entirely, in a dedicated directory under `weft_home` —
    /// so this test's hazard is no longer "a worktree-writable `.weft`"
    /// (there is no `.weft` in the worktree for this path anymore at all —
    /// asserted below) but the residual one `refuse_symlinks`'s own doc now
    /// names: a symlink planted directly at the Weft-managed output
    /// directory (by some OTHER same-uid process) is still refused, as
    /// defense in depth. Needs an ISOLATED `WEFT_HOME` (unlike this module's
    /// other worktree-path tests, which deliberately avoid touching it) —
    /// see `paths::ENV_LOCK`'s own doc for why every WEFT_HOME-touching test
    /// must hold it.
    #[cfg(unix)]
    #[tokio::test]
    async fn screenshot_and_audit_paths_both_refuse_a_symlinked_weft_managed_output_dir() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home =
            std::env::temp_dir().join(format!("weft-computer-srv-refuse-sym-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let repo_tmp = tempfile::tempdir().unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "r", &repo_tmp.path().to_string_lossy(), "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let direction = repo::create_direction(
            &db, thread.id, "task", "claude", r.id, "why", "impl-only", "main",
        )
        .await
        .unwrap();

        let wt_path = std::env::temp_dir().join(format!("weft-refuse-sym-wt-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("weft-refuse-sym-wt-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wt_path);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&wt_path).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let wt = repo::record_worktree(&db, r.id, direction.id, "b", &wt_path.to_string_lossy(), true, true, "")
            .await
            .unwrap();

        let dir_s = direction.id.to_string();
        // Plant the symlinks at the Weft-managed output directory THIS
        // session resolves to — deliberately NOT inside the worktree, which
        // this change moves output off of entirely.
        let session_dir = crate::paths::computer_output_root()
            .unwrap()
            .join(thread.id.to_string())
            .join(&dir_s)
            .join(format!("wt-{}", wt.id));
        std::fs::create_dir_all(&session_dir).unwrap();
        std::os::unix::fs::symlink(&outside, session_dir.join("screenshots")).unwrap();
        std::os::unix::fs::symlink(&outside, session_dir.join("computer-audit.jsonl")).unwrap();

        let screenshot_err = screenshot_out_dir(&db, thread.id, &dir_s, None).await.unwrap_err();
        assert!(screenshot_err.contains("symlink"), "{screenshot_err}");
        let audit_path = audit_log_path(&db, thread.id, &dir_s, None).await;
        assert!(audit_path.is_none(), "a symlinked output dir must refuse the audit path too");
        assert!(
            !outside.join("computer-audit.jsonl").exists() && !outside.join("screenshots").exists(),
            "must never write through the symlink"
        );

        // the worker lane must never touch the
        // worktree's own `.weft` at all anymore — there is nothing left to
        // `git_exclude` there either (that call is gone).
        assert!(
            !wt_path.join(".weft").exists(),
            "worker-lane computer-use output must never touch the worktree at all"
        );

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&wt_path);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    // —— parameter validation before the lease ——

    /// A `left_click` missing `coordinate`, already Full-granted (so `approve`
    /// decides silently), must be rejected on the missing argument WITHOUT
    /// ever touching the control lease.
    ///
    /// Note: `approve` itself resolves the
    /// target window authoritatively as part of authorizing this call,
    /// independent of whether `coordinate` is even
    /// present — so this test installs `shared_mock` with a resolvable
    /// "notes" window (under `process_state_test_lock`) to keep that
    /// resolve itself from being the thing that fails, so the assertion
    /// below still isolates the ACTUAL property under test: the missing-
    /// `coordinate` rejection inside the "left_click" arm itself.
    #[tokio::test]
    async fn missing_coordinate_click_never_touches_the_control_lease() {
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        let asks = AskRegistry::new();
        let thread = 904_001;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });
        // Shares the same process-wide control-lease static as every other
        // `computer::acquire_control`-touching test in this binary (see
        // `computer::mod`'s own tests' notes on the same hazard) — cleared
        // immediately before and after so this test's own assertion isn't
        // muddied by a lease some other test happened to leave behind.
        // that comment previously described the
        // hazard without actually preventing it — `process_state_test_lock`
        // does, by serializing this against every other test that touches
        // the same family of globals.
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 904_001,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, session_token_generation(thread, dir, None), "computer", "left_click",
            &json!({"window": "notes"}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(
            err.contains("coordinate"),
            "must fail on the missing coordinate, not a lease/busy error: {err}"
        );
        assert!(
            computer::control_state().is_none(),
            "a rejected missing-argument call must never touch the control lease"
        );
        computer::clear_control();
    }

    // —— MAX_TYPE_CHARS ——

    #[test]
    fn check_type_length_accepts_up_to_the_cap_and_rejects_over_it() {
        assert!(check_type_length(&"a".repeat(MAX_TYPE_CHARS)).is_ok());
        let err = check_type_length(&"a".repeat(MAX_TYPE_CHARS + 1)).unwrap_err();
        assert!(err.contains("too long"), "{err}");
        assert!(err.contains(&(MAX_TYPE_CHARS + 1).to_string()), "{err}");
    }

    #[test]
    fn check_type_length_counts_unicode_scalars_not_bytes() {
        // Mirrors `redact_audit_args_counts_unicode_scalars_not_bytes`'s own
        // reasoning: a multi-byte character must count as ONE char, not
        // however many bytes it takes in UTF-8, or a string well under the
        // human-meaningful cap could be rejected on byte length alone.
        let text = "é".repeat(MAX_TYPE_CHARS); // 2 bytes/char in UTF-8, 1 char each
        assert_eq!(text.chars().count(), MAX_TYPE_CHARS);
        assert!(check_type_length(&text).is_ok());
    }

    /// End-to-end through `run_action`'s own "type" arm (Full-granted, so
    /// `approve` decides silently): an over-limit `type` is rejected before
    /// EVER reaching the "type" arm's OWN window resolution — the one that
    /// records `window_id_out` and would go on to touch the focus-freshness
    /// gate, the control lease, and the backend call itself.
    ///
    /// Note: `approve` itself ALSO resolves the
    /// window authoritatively, unconditionally, for any Write action with a
    /// non-blank window argument — including THIS one — to bind an
    /// `ApprovedWindow` before this call ever reaches its own arm (see
    /// `bind_approved_window`'s doc). That resolve happens regardless of
    /// whether the LATER, action-specific arguments (like this call's
    /// over-limit `text`) turn out to be invalid — so this test installs
    /// `shared_mock` with a resolvable "notes" window (a variant without any
    /// backend at all would rely on the
    /// guaranteed-`Unsupported` `StubBackend` to prove ordering). What this
    /// test still proves: `check_type_length` rejects before the ARM's own
    /// resolve/backend call — `window_id_out` stays `None` and no INPUT
    /// method (`click`/`type_text`/`key`/...) ever reaches the mock, even
    /// though `approve`'s own (list-only, unrecorded) resolve already
    /// succeeded. The integration test in `tests/computer_mcp.rs` covers the
    /// same "never reaches the backend" property end-to-end.
    #[tokio::test]
    async fn run_action_rejects_an_over_limit_type_before_resolving_the_window() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 904_101,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        let asks = AskRegistry::new();
        let thread = 904_101;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        let over_limit = "a".repeat(MAX_TYPE_CHARS + 1);
        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, session_token_generation(thread, dir, None), "computer", "type",
            &json!({"window": "notes", "text": over_limit}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(err.contains("too long"), "{err}");
        assert!(
            window_id_out.is_none(),
            "the call must fail before the arm's OWN resolve ever records a window id: {window_id_out:?}"
        );
        assert!(
            mock.actions.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "an over-limit type must never reach the backend as an input action"
        );
    }

    // —— key combo validated before the lease ——

    /// End-to-end through `run_action`'s own "key" arm (Full-granted, so
    /// `approve` decides silently): a malformed combo (`ctrl+a+b` — a
    /// non-modifier in a non-final position, see `parse_key_combo`'s own
    /// grammar) is rejected before EVER touching the control lease/throttle
    /// or activating the target window — `computer::control_state()` must
    /// stay `None` and the mock backend must never see a `key`/`activate`
    /// call, exactly like `missing_coordinate_click_never_touches_the_
    /// control_lease`'s own property for the click family.
    #[tokio::test]
    async fn run_action_rejects_a_malformed_key_combo_before_touching_the_control_lease() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 904_201,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        let asks = AskRegistry::new();
        let thread = 904_201;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, session_token_generation(thread, dir, None), "computer", "key",
            &json!({"window": "notes", "text": "ctrl+a+b"}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(err.contains("ctrl") || err.to_lowercase().contains("key"), "{err}");
        assert!(
            mock.actions.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "a malformed combo must never reach the backend, not even an activate call"
        );
        assert!(
            computer::control_state().is_none(),
            "a rejected malformed combo must never touch the control lease"
        );
        computer::clear_control();
    }

    // —— check_suspended before opening a NEW card ——

    /// The property this fix exists for: a session with an ALREADY-open,
    /// unanswered permission card must reject a Write action BEFORE `approve`
    /// ever opens a SECOND card for it — not just after (the pre-existing
    /// in-arm `check_suspended` calls still catch a card racing in DURING
    /// `approve`'s own await, but this call never reaches that point at all).
    /// Deliberately UNGRANTED (no standing Full/Always grant): if `approve`
    /// ran first here, it would open a real Needs-you card of its own before
    /// this test could even answer it, hanging until `bus::server::ASK_WAIT`
    /// expires. Asserts all three: the pre-existing ask is still the ONLY
    /// open one (no second card), the backend was never touched, and no
    /// grant was recorded for the rejected call.
    #[tokio::test]
    async fn run_action_rejects_a_write_action_before_opening_a_second_card() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 904_301,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        let asks = AskRegistry::new();
        let thread = 904_301;
        let dir = "lead";
        // An unrelated card is already open for this EXACT (thread, dir) —
        // standing in for some other tool's own permission request racing
        // in first.
        let (existing_ask_id, _rx) = asks.request(
            thread, dir, "some_other_tool", "an unrelated permission request", "detail",
            crate::ask::RiskLevel::Unknown, "[\"unrelated\"]",
        );

        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, session_token_generation(thread, dir, None), "computer", "left_click",
            &json!({"window": "notes", "coordinate": [1, 1]}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(err.contains("permission card"), "{err}");
        assert_eq!(
            asks.open().len(),
            1,
            "no SECOND card may open while the original one is still unanswered"
        );
        assert_eq!(asks.open()[0].id, existing_ask_id, "the surviving open ask must be the ORIGINAL one");
        assert!(
            mock.actions.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "the backend must never be touched"
        );
        assert!(
            computer::control_state().is_none(),
            "no control lease may be taken for a call rejected before approve"
        );

        assert!(asks.answer(existing_ask_id, crate::ask::Answer::Deny));
        computer::clear_control();
    }

    #[tokio::test]
    async fn run_action_rejects_a_malformed_action_before_opening_any_card() {
        // a malformed call
        // (here `left_click` with no `coordinate`) must be rejected by pure
        // schema validation BEFORE `approve` — so NO permission card is ever
        // opened for it, even with no standing grant. If validation regressed
        // to run only inside the arm (after `approve`), this call would block
        // on a Needs-you card (no grant, no answer) up to `ASK_WAIT`; the 2s
        // timeout turns that regression into a clean fast failure, never a
        // 3600s hang.
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        let mock = shared_mock();
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 904_401,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        let asks = AskRegistry::new(); // no grant, no pre-existing open ask
        let thread = 904_401;
        let dir = "lead";

        let mut window_id_out = None;
        let mut image_out = None;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_action(
                &db, &asks, thread, dir, None, session_token_generation(thread, dir, None), "computer", "left_click",
                &json!({"window": "notes"}), // no `coordinate`
                &mut window_id_out, &mut image_out,
            ),
        )
        .await
        .expect("a malformed call must reject synchronously via pure_validate, never block on a card");

        let err = result.unwrap_err();
        assert!(err.contains("coordinate"), "expected a coordinate schema error, got: {err}");
        assert!(
            asks.open().is_empty(),
            "a malformed call must open NO approval card: {:?}",
            asks.open()
        );
        assert!(
            mock.actions.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "the backend must never be touched by a malformed call"
        );
        computer::clear_control();
    }

    /// a card published by a
    /// caller that straddled the disable transition (passed `run_action`'s
    /// `enabled` gate BEFORE the latch tripped, inserted its card AFTER
    /// `cancel_gui_asks`'s sweep) self-cancels via `approve`'s post-insert
    /// `stop_latched` check — leaving NO surviving card a human could answer
    /// Always/Full into a post-disable grant. Driven by calling `approve`
    /// directly with the latch already tripped (`wait` is Write-classified but
    /// windowless, so no backend is needed) — through `run_action`'s front
    /// door the top gate would already reject, which is exactly why only the
    /// straddler path can reach this insert.
    #[tokio::test]
    async fn approve_self_cancels_a_card_published_after_the_stop_latch_tripped() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        let asks = AskRegistry::new(); // no grant → would otherwise open a card
        let thread = 904_402;
        let dir = "lead";

        computer::trip_stop_latch();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            approve(&asks, thread, dir, None, "wait", &json!({"duration_ms": 5})),
        )
        .await
        .expect("the post-insert latch check must fail fast, never block on the card");

        let err = result.unwrap_err();
        assert!(err.contains("disabled"), "expected the disabled rejection, got: {err}");
        assert!(
            asks.open().is_empty(),
            "the straddler's card must be self-cancelled, not left open: {:?}",
            asks.open()
        );

        computer::clear_emergency_stop(computer::stop_generation());
    }

    /// the SAME straddler
    /// property for route revocation — a card inserted AFTER a delete's asks
    /// purge already swept must be self-cancelled by the post-insert
    /// revocation recheck, never left answerable (Always/Full into a deleted
    /// identity) for the full ask timeout.
    #[tokio::test]
    async fn approve_self_cancels_a_card_published_after_route_revocation() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        let asks = AskRegistry::new(); // no grant → would otherwise open a card
        let thread = 904_405;
        let restore = snapshot_revocations(&[thread]);
        revoke_computer_routes(thread);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            approve(&asks, thread, "lead", None, "wait", &json!({"duration_ms": 5})),
        )
        .await
        .expect("the post-insert revocation check must fail fast, never block on the card");

        let err = result.expect_err("a revoked route must self-cancel its own card");
        assert!(err.contains("no longer exists"), "{err}");
        assert!(
            asks.open().is_empty(),
            "the straddler's card must be self-cancelled, not left open: {:?}",
            asks.open()
        );

        restore_revocations(restore);
    }

    /// the authorization-
    /// time window resolve rechecks the stop latch (and route revocation)
    /// INSIDE its blocking closure — a windowed request that was parked on the
    /// screenshot semaphore or the blocking-pool queue when a human hit Stop
    /// must fail closed WITHOUT enumerating the desktop and without minting a
    /// card. Proven by seeding `windows_sequence` with one entry: had the
    /// closure reached `resolve_window`, `list_windows` would have popped it.
    #[tokio::test]
    async fn approve_authorization_resolve_fails_closed_after_stop_without_enumerating() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = None;
        {
            let mut seq = mock.windows_sequence.lock().unwrap_or_else(|e| e.into_inner());
            seq.clear();
            seq.push_back(vec![computer::WindowInfo {
                id: 904_501,
                app: "Notes".into(),
                title: "notes".into(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            }]);
        }
        computer::clear_emergency_stop(computer::stop_generation());
        let asks = AskRegistry::new();

        computer::trip_stop_latch();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            approve(
                &asks,
                904_501,
                "lead",
                None,
                "screenshot",
                &json!({"action": "screenshot", "window": "notes"}),
            ),
        )
        .await
        .expect("the in-closure latch recheck must fail fast, never block on a card");

        let err = result.expect_err("a tripped stop latch must fail the resolve closed");
        assert!(err.contains("disabled"), "expected the disabled rejection, got: {err}");
        assert!(asks.open().is_empty(), "no card may be minted after Stop: {:?}", asks.open());
        assert_eq!(
            mock.windows_sequence.lock().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "the closure must refuse BEFORE resolve_window ever enumerates (nothing popped)"
        );

        mock.windows_sequence.lock().unwrap_or_else(|e| e.into_inner()).clear();
        computer::clear_emergency_stop(computer::stop_generation());
    }

    /// A grant-less call whose window query resolves to NOTHING must not
    /// learn that before authorization — an immediate `WindowNotFound`
    /// return would be a window-existence oracle (instant error vs. approval
    /// wait = one bit of desktop enumeration per probe, no human involved).
    /// The failure opens a query-only card (no preview) and the error text
    /// reaches the agent only AFTER the human answers Allow.
    #[tokio::test]
    async fn a_failed_window_resolve_defers_behind_a_card_and_surfaces_only_after_allow() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        // An EMPTY desktop: any query fails to resolve.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![]);
        computer::clear_emergency_stop(computer::stop_generation());
        let asks = AskRegistry::new(); // no grant
        let thread = 904_601;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "ghost", "coordinate": [1, 1]});
        let digest = args_digest(&args);

        let asks_bg = asks.clone();
        let args_bg = args.clone();
        let handle = tokio::spawn(async move {
            approve(&asks_bg, thread, dir, None, "left_click", &args_bg).await
        });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect(
            "a failed resolve must open a card for the probe itself, never return the error \
             straight to the grant-less agent",
        );
        assert!(
            !handle.is_finished(),
            "the call must be WAITING on the card — an early return is the existence oracle"
        );
        assert_eq!(
            card.action_key,
            crate::ask::action_key(&["gui", "left_click", "ghost", &digest]),
            "an unresolved window keys the card by the QUERY alone — there is no identity to bind"
        );
        assert!(card.preview.is_none(), "no window resolved, so there is nothing to preview");

        assert!(asks.answer(card.id, crate::ask::Answer::Allow));
        let err = handle.await.unwrap().expect_err(
            "even after Allow the action must fail — approval could not pin down a window identity",
        );
        assert!(err.contains("no visible window matched"), "{err}");

        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The Deny half of the existence-oracle gate: denying the probe's card
    /// returns the exact same "denied in weft" every other deny returns —
    /// leaking nothing about whether the window existed.
    #[tokio::test]
    async fn a_failed_window_resolve_denied_card_returns_the_ordinary_deny() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![]);
        computer::clear_emergency_stop(computer::stop_generation());
        let asks = AskRegistry::new();
        let thread = 904_602;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "ghost", "coordinate": [1, 1]});

        let asks_bg = asks.clone();
        let args_bg = args.clone();
        let handle = tokio::spawn(async move {
            approve(&asks_bg, thread, dir, None, "left_click", &args_bg).await
        });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect("the failed resolve must card, same as the Allow test");

        assert!(asks.answer(card.id, crate::ask::Answer::Deny));
        let err = handle.await.unwrap().expect_err("a denied card must deny the call");
        assert_eq!(err, "denied in weft", "a deny must be byte-identical to any other deny");
        assert!(
            !err.contains("window"),
            "the deny must not carry any window-existence information"
        );

        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// A query-only Always grant (mintable ONLY from a resolution-failure
    /// card a human already approved) fast-paths future failing resolves of
    /// that same query: the error returns without a fresh card. Successful
    /// resolves can never match it — their keys carry id/app/title — so the
    /// grant authorizes exactly the disclosure the human already made, and
    /// nothing else.
    #[tokio::test]
    async fn a_query_only_always_grant_returns_the_resolve_failure_without_a_new_card() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![]);
        computer::clear_emergency_stop(computer::stop_generation());
        let asks = AskRegistry::new();
        let thread = 904_603;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "ghost", "coordinate": [1, 1]});
        let digest = args_digest(&args);
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: Vec::new(),
            always: vec![crate::ask::AlwaysGrant {
                thread,
                dir: dir.to_string(),
                action_key: crate::ask::action_key(&["gui", "left_click", "ghost", &digest]),
            }],
        });

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            approve(&asks, thread, dir, None, "left_click", &args),
        )
        .await
        .expect("a standing grant must decide without blocking on any card");

        let err = result.expect_err("the deferred resolve failure surfaces under the grant");
        assert!(err.contains("no visible window matched"), "{err}");
        assert!(asks.open().is_empty(), "the grant path must mint no card: {:?}", asks.open());

        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// `pace_activate_verify_and_inject` re-runs the synchronous stop/lease
    /// recheck INSIDE its paced closure, immediately before the activation
    /// call — a Stop that lands while the closure sits queued (or during the
    /// pacing sleep) must prevent the target window from ever being
    /// raised/focused, not merely the click/type that would follow — and its
    /// `tail` (the arm's mapping + backend injection) must never run either.
    #[tokio::test]
    async fn activation_fails_closed_after_stop_without_raising_the_target_window() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        computer::clear_emergency_stop(computer::stop_generation());
        let asks = AskRegistry::new();

        computer::trip_stop_latch();
        let target = computer::WindowInfo {
            id: 42,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        };
        let mut window_id_out = None;
        let tail_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tail_flag = tail_ran.clone();
        let err = pace_activate_verify_and_inject(
            &asks,
            904_502,
            "lead",
            None,
            session_token_generation(904_502, "lead", None),
            "notes",
            &None,
            target,
            &mut window_id_out,
            move |_| {
                tail_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect_err("a tripped stop latch must fail activation closed");
        assert!(err.contains("disabled"), "expected the disabled rejection, got: {err}");
        assert!(
            mock.actions.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "the closure must refuse BEFORE activate_window raises/focuses anything"
        );
        assert!(
            !tail_ran.load(std::sync::atomic::Ordering::SeqCst),
            "the injection tail must never run once the stop latch is tripped"
        );

        computer::clear_emergency_stop(computer::stop_generation());
    }

    /// A Needs-you card that opens DURING the paced closure's own
    /// `resolve_and_verify_target` enumeration — AFTER the closure's first
    /// `has_open` check has already passed — must still suspend the
    /// injection: the FINAL `has_open` check, run immediately before `tail`,
    /// catches it and `tail` never touches the backend. Isolates that
    /// second check specifically by driving the helper directly (one
    /// resolve) with NO card open at entry, then opening one from inside
    /// `list_windows` via `on_list_windows` — the enumeration-time analogue
    /// of the `on_activate` hook.
    #[tokio::test]
    async fn a_card_opening_during_the_in_closure_resolve_suspends_the_injection() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let target = computer::WindowInfo {
            id: 906_601,
            app: "Bar".into(),
            title: "Bar".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        };
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(vec![target.clone()]);

        let asks = AskRegistry::new();
        let thread = 906_601;
        let dir = "lead";
        // The helper's post-activation `renew_lease_after_queue` requires
        // THIS (thread, dir) to still hold the lease — take it so the closure
        // reaches the resolve (and thus the final has_open check) rather than
        // failing at the lease gate first.
        computer::acquire_control(thread, dir, None).unwrap();

        // No card is open at entry — the closure's FIRST has_open check (right
        // after activation) passes. The hook then opens one from inside the
        // in-closure resolve's `list_windows`, so ONLY the final check can
        // catch it. One-shot: clear the hook the first time it fires so the
        // resolve itself still returns a window set on that same call.
        let asks_hook = asks.clone();
        // One-shot via an atomic flag, NOT by clearing `on_list_windows` from
        // inside the hook: `list_windows` invokes the hook while still holding
        // that very mutex, so re-locking it here would deadlock (std `Mutex`
        // is not reentrant).
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        *mock.on_list_windows.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Box::new(move || {
                if fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                // an UNRELATED permission request racing in for this same
                // (thread, dir) during the enumeration. The returned waiter
                // is dropped — the OPEN ask (what `has_open` sees) is what
                // matters here, not its eventual answer.
                let _ = asks_hook.request(thread, dir, "bash", "Run: rm -rf /", "rm -rf /", crate::ask::RiskLevel::Write, "k");
            }));

        let mut window_id_out = None;
        let tail_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tail_flag = tail_ran.clone();
        let err = pace_activate_verify_and_inject(
            &asks,
            thread,
            dir,
            None,
            session_token_generation(thread, dir, None),
            "Bar",
            &None,
            target,
            &mut window_id_out,
            move |_| {
                tail_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect_err("a card opening during the resolve must suspend the injection");

        assert!(
            err.contains("waiting on your input") || err.to_lowercase().contains("suspend") || err.contains("pending"),
            "expected the suspended-pending-ask rejection, got: {err}"
        );
        assert!(
            !tail_ran.load(std::sync::atomic::Ordering::SeqCst),
            "the injection tail must never run while a Needs-you card is open"
        );
        assert!(
            !mock.actions.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|a| a.starts_with("type ") || a.starts_with("click ")),
            "the backend must receive no input once a card is open"
        );

        *mock.on_list_windows.lock().unwrap_or_else(|e| e.into_inner()) = None;
        computer::clear_control();
    }

    /// A Needs-you card that is already open when the paced closure runs
    /// must stop it BEFORE `activate_target` — raising/focusing the target
    /// application would throw a window over the very card the human is
    /// being asked to read (and pull focus off it), which is the same
    /// "no desktop-facing action while a card is pending" rule
    /// `check_suspended` enforces everywhere else. Proven by asserting the
    /// backend recorded NO `activate` action at all.
    #[tokio::test]
    async fn a_pending_card_stops_the_paced_closure_before_it_activates_anything() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *mock.on_list_windows.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let target = computer::WindowInfo {
            id: 906_701,
            app: "Bar".into(),
            title: "Bar".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        };
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(vec![target.clone()]);

        let asks = AskRegistry::new();
        let thread = 906_701;
        let dir = "lead";
        computer::acquire_control(thread, dir, None).unwrap();
        // A card is already open for this (thread, dir) — standing in for one
        // that opened during the pacing sleep or the blocking-pool queue.
        let _pending = asks.request(
            thread, dir, "bash", "Run: rm -rf /", "rm -rf /", crate::ask::RiskLevel::Write, "k",
        );

        let mut window_id_out = None;
        let tail_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tail_flag = tail_ran.clone();
        let err = pace_activate_verify_and_inject(
            &asks,
            thread,
            dir,
            None,
            session_token_generation(thread, dir, None),
            "Bar",
            &None,
            target,
            &mut window_id_out,
            move |_| {
                tail_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect_err("a pending card must stop the closure");

        let actions = mock.actions.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !actions.iter().any(|a| a.starts_with("activate")),
            "the target window must NEVER be raised/focused while a card is open: {actions:?}"
        );
        drop(actions);
        assert!(
            !tail_ran.load(std::sync::atomic::Ordering::SeqCst),
            "the injection tail must never run while a card is open"
        );
        assert!(err.contains("permission card"), "{err}");

        computer::clear_control();
    }

    /// grant-less OBSERVE
    /// calls stop opening cards once [`MAX_OPEN_OBSERVE_ASKS`] are already
    /// open for this (thread, dir) — the one-over call fails closed with the
    /// backlog error instead of minting an unbounded Ask + waiter per call.
    /// `list_windows` is used (ReadOnly, windowless — no backend resolution),
    /// with each under-cap call parked on its own spawned task awaiting its
    /// card.
    #[tokio::test]
    async fn observe_asks_are_capped_per_session_once_the_backlog_is_full() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        let asks = AskRegistry::new();
        let thread = 904_403;
        let dir = "lead";

        let mut parked = Vec::new();
        for _ in 0..MAX_OPEN_OBSERVE_ASKS {
            let a = asks.clone();
            parked.push(tokio::spawn(async move {
                let _ = approve(&a, thread, dir, None, "list_windows", &json!({})).await;
            }));
        }
        // Wait (bounded) until all under-cap cards are actually open.
        for _ in 0..200 {
            if asks.open().len() == MAX_OPEN_OBSERVE_ASKS {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(asks.open().len(), MAX_OPEN_OBSERVE_ASKS, "precondition: backlog full");

        // The one-over call must reject immediately — no fourth card.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            approve(&asks, thread, dir, None, "list_windows", &json!({})),
        )
        .await
        .expect("an over-cap observe call must fail fast, never open another card");
        let err = result.unwrap_err();
        assert!(err.contains("too many"), "expected the backlog rejection, got: {err}");
        assert_eq!(asks.open().len(), MAX_OPEN_OBSERVE_ASKS, "no card past the cap");

        // Unblock and reap the parked callers.
        for a in asks.open() {
            asks.cancel(a.id);
        }
        for p in parked {
            let _ = p.await;
        }
    }

    // —— multi-worktree `wt` routing ——

    /// `session_root` no longer returns the
    /// worktree's OWN path for the worker lane — it returns a dedicated
    /// namespace under `weft_home/computer/<thread>/<dir>/wt-<id>` (see that
    /// function's own doc). This test now needs an ISOLATED `WEFT_HOME` to
    /// assert deterministic paths — see `paths::ENV_LOCK`'s own doc for why
    /// every WEFT_HOME-touching test must hold it.
    #[tokio::test]
    async fn session_root_wt_pins_the_exact_worktree_and_a_foreign_one_fails_closed() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home = std::env::temp_dir()
            .join(format!("weft-computer-srv-session-root-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        let repo_a = repo::add_repo_ref(&db, ws.id, "a", &tmp_a.path().to_string_lossy(), "main", "", true)
            .await
            .unwrap();
        let repo_b = repo::add_repo_ref(&db, ws.id, "b", &tmp_b.path().to_string_lossy(), "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let direction = repo::create_direction(
            &db, thread.id, "task", "claude", repo_a.id, "why", "impl-only", "main",
        )
        .await
        .unwrap();

        // Two worktrees for the SAME (multi-repo) direction — the exact
        // shape the `wt` pin exists for: `.next()` alone would always
        // resolve to whichever of these was inserted first, regardless of
        // which worker session actually asked.
        let wt_a =
            repo::record_worktree(&db, repo_a.id, direction.id, "b1", "/tmp/weft-wt-a", true, true, "")
                .await
                .unwrap();
        let wt_b =
            repo::record_worktree(&db, repo_b.id, direction.id, "b2", "/tmp/weft-wt-b", true, true, "")
                .await
                .unwrap();
        let dir_s = direction.id.to_string();
        let expected_root_for = |wt_id: i32| {
            crate::paths::computer_output_root()
                .unwrap()
                .join(thread.id.to_string())
                .join(&dir_s)
                .join(format!("wt-{wt_id}"))
        };

        // An explicit `wt` pins the EXACT worktree's OWN weft-managed
        // namespace, even though it was inserted SECOND.
        let root = session_root(&db, thread.id, &dir_s, Some(wt_b.id)).await.unwrap();
        assert_eq!(root, expected_root_for(wt_b.id));

        // No `wt` at all: unchanged pre-existing "first worktree" fallback —
        // still resolves to wt_a's OWN namespace, keeping the SAME
        // per-worktree isolation the old worktree-rooted path got for free
        // : two worktrees of the same multi-repo direction
        // never share an output namespace, whether `wt` came from an
        // explicit pin or this fallback.
        let no_wt = session_root(&db, thread.id, &dir_s, None).await.unwrap();
        assert_eq!(no_wt, expected_root_for(wt_a.id));
        assert_ne!(
            root, no_wt,
            "wt_a and wt_b must never resolve to the same output namespace"
        );

        // A `wt` naming a worktree of a DIFFERENT direction is rejected
        // (closed-set validation) — this must FAIL CLOSED
        // (`None`), never silently fall back to "first worktree of THIS
        // direction" as it used to. A worker session that explicitly pinned a
        // worktree that's since become invalid must never have its
        // screenshots/audit quietly redirected into a DIFFERENT repo's
        // namespace in this multi-repo direction.
        let other_direction = repo::create_direction(
            &db, thread.id, "task2", "claude", repo_a.id, "why", "impl-only", "main",
        )
        .await
        .unwrap();
        let foreign = repo::record_worktree(
            &db, repo_a.id, other_direction.id, "b3", "/tmp/weft-wt-foreign", true, true, "",
        )
        .await
        .unwrap();

        let fails_closed = session_root(&db, thread.id, &dir_s, Some(foreign.id)).await;
        assert_eq!(
            fails_closed, None,
            "an explicit wt naming a worktree of a DIFFERENT direction must fail closed, \
             never silently resolve to (or fall back onto) any worktree at all"
        );

        // A wt naming a worktree that's simply been deleted (never recorded
        // at all, here) must fail closed the exact same way.
        let deleted = session_root(&db, thread.id, &dir_s, Some(999_999)).await;
        assert_eq!(deleted, None, "an explicit wt with no matching worktree row must fail closed");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    // —— bounded preview registry ——

    /// A throwaway [`VerifiedWindowIdentity`] for tests that only exercise the
    /// registry's capacity/timestamp behavior, where the window identity itself
    /// is irrelevant
    /// .
    fn pid(id: u32) -> VerifiedWindowIdentity {
        VerifiedWindowIdentity { id, app: String::new(), title: String::new() }
    }

    #[test]
    fn evict_oldest_if_full_removes_only_the_single_oldest_entry_at_capacity() {
        let mut map: HashMap<(i32, String, Option<i32>), (String, VerifiedWindowIdentity, u64)> =
            HashMap::new();
        for i in 0..MAX_PREVIEWS as i32 {
            map.insert((i, "d".to_string(), None), (format!("p{i}"), pid(i as u32), i as u64));
        }
        assert_eq!(map.len(), MAX_PREVIEWS);

        // Simulate the (MAX_PREVIEWS + 1)th write: evict, then insert.
        evict_oldest_if_full(&mut map);
        map.insert(
            (MAX_PREVIEWS as i32, "d".to_string(), None),
            (format!("p{MAX_PREVIEWS}"), pid(MAX_PREVIEWS as u32), MAX_PREVIEWS as u64),
        );

        assert_eq!(map.len(), MAX_PREVIEWS, "capacity must stay bounded");
        assert!(
            !map.contains_key(&(0, "d".to_string(), None)),
            "the OLDEST entry (ts=0) must be evicted"
        );
        for i in 1..=MAX_PREVIEWS as i32 {
            assert!(
                map.contains_key(&(i, "d".to_string(), None)),
                "every entry newer than the evicted one must remain: missing {i}"
            );
        }
    }

    #[test]
    fn evict_oldest_if_full_is_a_no_op_below_capacity() {
        let mut map: HashMap<(i32, String, Option<i32>), (String, VerifiedWindowIdentity, u64)> =
            HashMap::new();
        map.insert((1, "d".to_string(), None), ("p".to_string(), pid(1), 100));
        evict_oldest_if_full(&mut map);
        assert_eq!(map.len(), 1, "must not evict anything below capacity");
    }

    #[test]
    fn store_screenshot_preview_updating_an_existing_key_never_evicts_anyone() {
        // Fill to capacity with distinct (thread, dir) keys, then refresh the
        // FIRST one again (same key) — this must never trigger an eviction,
        // since it doesn't grow the map.
        for i in 0..MAX_PREVIEWS as i32 {
            store_screenshot_preview(910_000 + i, "lead", None, format!("p{i}"), pid(i as u32));
        }
        store_screenshot_preview(910_000, "lead", None, "refreshed".to_string(), pid(999));

        let g = screenshot_previews().lock().unwrap();
        assert!(
            g.len() <= MAX_PREVIEWS,
            "refreshing an existing key must never grow past capacity: {}",
            g.len()
        );
        assert_eq!(
            g.get(&(910_000, "lead".to_string(), None)).map(|(p, ..)| p.clone()),
            Some("refreshed".to_string()),
            "the refreshed value must actually be stored"
        );
    }

    /// the preview registry
    /// stores and matches the FULL window identity (id + app + title), so a
    /// reused numeric id belonging to a DIFFERENT window (different app/title)
    /// no longer matches a stale preview — the id-reuse hazard an id-only
    /// comparison left open.
    #[test]
    fn screenshot_preview_stores_and_matches_the_full_window_identity() {
        let captured = VerifiedWindowIdentity { id: 5, app: "Notes".into(), title: "todo".into() };
        let id_reused = VerifiedWindowIdentity { id: 5, app: "Mail".into(), title: "inbox".into() };
        assert_ne!(
            captured, id_reused,
            "the SAME numeric id with a different app/title must NOT compare equal"
        );

        // Unique (thread, dir, wt) key — no process-wide lock needed (see
        // `process_state_test_lock`'s doc on keyed vs unkeyed state).
        store_screenshot_preview(920_001, "lead", None, "PREVIEW".to_string(), captured.clone());
        let (data, stored) = last_screenshot_preview(920_001, "lead", None).unwrap();
        assert_eq!(data, "PREVIEW", "the preview data URI round-trips");
        assert_eq!(stored, captured, "the FULL identity round-trips, not just the id");
        assert_ne!(
            stored, id_reused,
            "a later window that merely reused the numeric id won't match the stored preview"
        );
    }

    /// sibling workers of
    /// ONE direction share `(thread, dir)` but are distinct sessions keyed by
    /// `wt` — one sibling's preview must never be readable under (or attach to
    /// a card for) another's key, the same isolation `computer::shot_dims_for`
    /// has.
    #[test]
    fn screenshot_previews_are_isolated_per_worktree_for_sibling_workers() {
        let identity = pid(42);
        store_screenshot_preview(930_001, "0", Some(1), "SIBLING-A".to_string(), identity.clone());

        assert!(
            last_screenshot_preview(930_001, "0", Some(2)).is_none(),
            "a sibling worker differing only by wt must not see another's preview"
        );
        assert!(
            last_screenshot_preview(930_001, "0", None).is_none(),
            "the lead lane (wt=None) must not see a worker's preview either"
        );
        let (data, stored) = last_screenshot_preview(930_001, "0", Some(1)).unwrap();
        assert_eq!(data, "SIBLING-A", "the owning (thread, dir, wt) still reads its own entry");
        assert_eq!(stored, identity);
    }
}
