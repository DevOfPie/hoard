//! Machine-readable output (`--json`): the contract agents parse.
//!
//! With `--json`, stdout carries exactly one JSON envelope and nothing else:
//! `{"ok":true,"data":...}` when the command succeeds, `{"ok":false,"error":...}`
//! when it fails, with a non-zero exit code. Human logs keep going to stderr, so
//! an agent that reads only stdout always gets valid JSON, failure included, which
//! is why the error envelope goes to stdout too.
//!
//! The shapes live here and are never borrowed from `hoard-agent`. Serializing an
//! engine struct straight to stdout would turn its fields into public API and make
//! every refactor a breaking change for the agents parsing us. Same append-only
//! discipline as the IPC wire: add fields, never repurpose or remove one, and bump
//! the agent contract when the surface really changes.

use anyhow::Result;
use hoard_agent::api::ApiError;
use serde::Serialize;
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once from `main` after parsing the global `--json` flag.
static JSON: AtomicBool = AtomicBool::new(false);

pub fn set_json(on: bool) {
    JSON.store(on, Ordering::Relaxed);
}

pub fn json() -> bool {
    JSON.load(Ordering::Relaxed)
}

#[derive(Serialize)]
struct Ok_<'a, T> {
    ok: bool,
    data: &'a T,
}

#[derive(Serialize)]
struct Err_<'a> {
    ok: bool,
    error: ErrBody<'a>,
}

#[derive(Serialize)]
struct ErrBody<'a> {
    code: &'a str,
    message: String,
    /// Only on `rate_limited`, so a caller waits the window the server named
    /// instead of guessing a backoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_seconds: Option<u32>,
}

/// Render a command's result: the JSON envelope under `--json`, otherwise
/// whatever `human` prints. Every command that returns data goes through here,
/// so no command can forget to honour the flag.
pub fn emit<T: Serialize>(value: &T, human: impl FnOnce(&T)) -> Result<()> {
    if json() {
        let env = Ok_ {
            ok: true,
            data: value,
        };
        println!("{}", serde_json::to_string_pretty(&env)?);
    } else {
        human(value);
    }
    Ok(())
}

/// An error the CLI raises itself, carrying the stable `code` callers branch on.
/// Errors coming up from the agent are classified in [`classify`] instead; this is
/// for the cases only the CLI knows about.
#[derive(Debug)]
pub struct Coded {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for Coded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Coded {}

/// Build a coded error: `return Err(output::err("not_tracked", "…"))`.
pub fn err(code: &'static str, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Coded {
        code,
        message: message.into(),
    })
}

/// Whether a prompt would actually reach a person.
///
/// False under `--json`, in a pipe, or with stdin redirected, where a prompt is
/// not a question but a hang: the caller is a script or an assistant, and it will
/// sit there until something times out. Commands ask this before printing anything
/// that waits on stdin, and fail with a coded error instead.
///
/// This doubles as the guard on destructive commands: no terminal means nobody is
/// there to say yes, so `--yes` has to be explicit.
pub fn interactive() -> bool {
    use std::io::IsTerminal;
    !json() && std::io::stdin().is_terminal()
}

/// `CliConfig::require_token`, coded. "You are not signed in" is the most
/// common thing a caller has to be told, and as a generic error it is
/// guesswork: every command that needs a session goes through here so the
/// answer is always `no_session` with exit 2.
pub fn require_token(cfg: &hoard_agent::config::CliConfig) -> Result<String> {
    cfg.require_token()
        .map(|t| t.to_string())
        .map_err(|e| err("no_session", format!("{e:#}")))
}

/// A failure, sorted into something a caller can act on.
pub struct Classified {
    /// Stable vocabulary. New codes may appear; existing ones don't change
    /// meaning. Owned only when the service relays a server tag.
    pub code: Cow<'static, str>,
    /// Grouped so a shell script can branch without parsing JSON. Codes within a
    /// group share a reaction, which is the whole point of the grouping:
    /// 2 sign in, 3 it isn't there, 4 wait, 5 free space or upgrade,
    /// 6 the network, 1 everything else.
    pub exit: i32,
    /// Present on 429: how long the server asked us to wait.
    pub retry_after_seconds: Option<u32>,
}

/// Sort an error into a code, an exit status and (for 429) a wait.
pub fn classify(e: &anyhow::Error) -> Classified {
    let plain = |code: &'static str, exit: i32| Classified {
        code: Cow::Borrowed(code),
        exit,
        retry_after_seconds: None,
    };

    if let Some(c) = e.downcast_ref::<Coded>() {
        let exit = match c.code {
            "no_session" => 2,
            "not_tracked" => 3,
            _ => 1,
        };
        return plain(c.code, exit);
    }

    // Refusals relayed by the service keep the server's tag: a 409 arrives
    // with the code the daemon names (`held`, `stale`, `lease_required`,
    // `not_shared`, `pushed`, `conflict`), still exit 1, and a request the user has to
    // change is `bad_request`. Any other refusal is relayed with its code and
    // grouped as the HTTP road groups the same answer. A service with no
    // session to act with is the sign-in group. Every other service failure
    // stays generic; its message already says what happened.
    match e.downcast_ref::<hoard_core::ipc::IpcError>() {
        Some(hoard_core::ipc::IpcError::Conflict { code, .. }) => {
            // An untagged 409 is still a conflict, never an empty code.
            let code = if code.is_empty() {
                Cow::Borrowed("conflict")
            } else {
                Cow::Owned(code.clone())
            };
            return Classified {
                code,
                exit: 1,
                retry_after_seconds: None,
            };
        }
        Some(hoard_core::ipc::IpcError::Refused { code, .. }) => {
            return Classified {
                code: Cow::Owned(code.clone()),
                exit: refused_exit(code),
                retry_after_seconds: None,
            };
        }
        Some(hoard_core::ipc::IpcError::Invalid { .. }) => return plain("bad_request", 1),
        // A missing or expired session, or a keyring that will not hand the
        // session over, is the sign-in group: signing in again rewrites the
        // keyring item under the service. An engine still starting, shutting
        // down or failing is not fixed by signing in.
        Some(
            hoard_core::ipc::IpcError::EngineDown {
                kind:
                    hoard_core::ipc::EngineDownReason::NoSession
                    | hoard_core::ipc::EngineDownReason::SessionExpired
                    | hoard_core::ipc::EngineDownReason::KeyringUnreadable,
                ..
            }
            | hoard_core::ipc::IpcError::NoServerSession { .. }
            | hoard_core::ipc::IpcError::CloudSessionExpired { .. },
        ) => return plain("no_session", 2),
        // A service older than `kind` sends none, and it reads `Unknown`. Its
        // text is the only way left to keep the sign-in hint for "no session".
        Some(hoard_core::ipc::IpcError::EngineDown {
            kind: hoard_core::ipc::EngineDownReason::Unknown,
            reason,
        }) if reason.to_ascii_lowercase().contains("no session") => return plain("no_session", 2),
        Some(hoard_core::ipc::IpcError::EngineDown { .. }) => return plain("engine_down", 1),
        _ => {}
    }

    match e.downcast_ref::<ApiError>() {
        Some(ApiError::Unauthorized) => plain("unauthorized", 2),
        Some(ApiError::Forbidden) => plain("forbidden", 2),
        Some(ApiError::NotFound) => plain("not_found", 3),
        // Nothing to trim and nothing to wait for: the account is at its limit
        // until the user frees space or upgrades. Same group as an oversized
        // save, which is the other "this will fail identically next time".
        Some(ApiError::QuotaExceeded(_)) => plain("quota_exceeded", 5),
        Some(ApiError::TooLarge(_)) => plain("too_large", 5),
        Some(ApiError::Archived) => plain("archived", 5),
        Some(ApiError::RateLimited {
            retry_after_seconds,
            ..
        }) => Classified {
            code: Cow::Borrowed("rate_limited"),
            exit: 4,
            retry_after_seconds: Some(*retry_after_seconds),
        },
        Some(ApiError::Network(_)) => plain("network", 6),
        Some(ApiError::StorageUnreachable { .. }) => plain("storage_unreachable", 6),
        // Same class and exit code as any other 409: `--json` is a contract,
        // and a non-fast-forward is still "conflict" to whoever is scripting us.
        Some(ApiError::NonFastForward(_)) => plain("conflict", 1),
        Some(ApiError::LeaseHeld(_))
        | Some(ApiError::LeaseStale(_))
        | Some(ApiError::LeaseRequired(_))
        | Some(ApiError::LeasePushed(_))
        | Some(ApiError::NotShared) => plain("conflict", 1),
        Some(ApiError::Conflict(_)) => plain("conflict", 1),
        Some(ApiError::BadRequest(_)) => plain("bad_request", 1),
        Some(ApiError::Server { .. }) => plain("server", 1),
        None => plain("error", 1),
    }
}

/// The exit group of a refusal the service relays, by its code: the same group
/// the matching `ApiError` gets. A throttle's wait is in its message only.
fn refused_exit(code: &str) -> i32 {
    match code {
        "unauthorized" | "forbidden" => 2,
        "not_found" | "not_watched" => 3,
        "throttled" => 4,
        "quota_full" => 5,
        // `bad_request`, `not_shared`, `needs_input` and whatever a newer
        // service sends.
        _ => 1,
    }
}

/// `s` cut to `max` characters for a table cell, the last one an ellipsis when
/// anything was dropped. Tables only: JSON never truncates.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Print the failure envelope (`--json`) or the plain `error: …` line.
/// Returns the exit code `main` should use.
pub fn emit_error(e: &anyhow::Error) -> i32 {
    let c = classify(e);
    if json() {
        let env = Err_ {
            ok: false,
            error: ErrBody {
                code: &c.code,
                message: format!("{e:#}"),
                retry_after_seconds: c.retry_after_seconds,
            },
        };
        // Never let a serializer bug swallow the error itself.
        match serde_json::to_string_pretty(&env) {
            Ok(s) => println!("{s}"),
            Err(_) => eprintln!("error: {e:#}"),
        }
    } else {
        eprintln!("error: {e:#}");
    }
    c.exit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coded_errors_keep_their_code() {
        let e = err("not_tracked", "nope");
        let c = classify(&e);
        assert_eq!(c.code, "not_tracked");
        assert_eq!(c.exit, 3);
    }

    #[test]
    fn a_429_carries_the_wait() {
        let e = anyhow::Error::new(ApiError::RateLimited {
            kind: hoard_agent::api::RateLimitKind::Budget,
            retry_after_seconds: 3600,
            body: String::new(),
        });
        let c = classify(&e);
        assert_eq!(c.code, "rate_limited");
        assert_eq!(c.exit, 4);
        // Without this an agent retries a wait it can't see, which is the loop
        // the server's brake exists to stop.
        assert_eq!(c.retry_after_seconds, Some(3600));
    }

    #[test]
    fn an_unclassified_error_is_generic_not_a_panic() {
        let c = classify(&anyhow::anyhow!("something odd"));
        assert_eq!(c.code, "error");
        assert_eq!(c.exit, 1);
    }

    /// A 409 the service relays keeps the daemon's own tag, so a script can
    /// tell `held` from `stale` without parsing the sentence; exit 1 as any
    /// conflict. It prints as the server's one line, not as the variant.
    /// Changed on purpose: this used to flatten every tag to `conflict`.
    #[test]
    fn a_relayed_conflict_keeps_its_code() {
        use hoard_core::ipc::IpcError;
        let e = anyhow::Error::new(IpcError::Conflict {
            code: "held".into(),
            message: "another member is hosting this save".into(),
        });
        assert_eq!(classify(&e).code, "held");
        assert_eq!(classify(&e).exit, 1);
        for code in [
            "stale",
            "lease_required",
            "not_shared",
            "pushed",
            "conflict",
        ] {
            let e = anyhow::Error::new(IpcError::Conflict {
                code: code.into(),
                message: "no".into(),
            });
            assert_eq!(classify(&e).code, code);
            assert_eq!(classify(&e).exit, 1);
        }
        assert_eq!(format!("{e:#}"), "another member is hosting this save");
        let e = anyhow::Error::new(IpcError::Conflict {
            code: String::new(),
            message: "no".into(),
        });
        assert_eq!(classify(&e).code, "conflict");
        let e = anyhow::Error::new(IpcError::Invalid {
            message: "`a/b` is not a world name".into(),
        });
        assert_eq!(classify(&e).code, "bad_request");
        assert_eq!(format!("{e:#}"), "`a/b` is not a world name");
    }

    /// Every code the service relays lands in the group its HTTP twin does,
    /// keeps its own name, and prints as the service's message.
    #[test]
    fn relayed_refusals_keep_their_code_and_group() {
        use hoard_core::ipc::IpcError;
        for (code, exit) in [
            ("unauthorized", 2),
            ("forbidden", 2),
            ("not_found", 3),
            ("not_watched", 3),
            ("throttled", 4),
            ("quota_full", 5),
            ("bad_request", 1),
            ("not_shared", 1),
            ("needs_input", 1),
            ("something_newer", 1),
        ] {
            let e = anyhow::Error::new(IpcError::Refused {
                code: code.into(),
                message: format!("refused: {code}"),
            });
            let c = classify(&e);
            assert_eq!((c.code.as_ref(), c.exit), (code, exit), "{code}");
            assert_eq!(c.retry_after_seconds, None);
            assert_eq!(format!("{e:#}"), format!("refused: {code}"));
        }
        for e in [
            IpcError::EngineDown {
                reason: "no session".into(),
                kind: hoard_core::ipc::EngineDownReason::NoSession,
            },
            IpcError::NoServerSession {
                reason: "none".into(),
            },
            IpcError::CloudSessionExpired {
                reason: "revoked".into(),
            },
        ] {
            let c = classify(&anyhow::Error::new(e));
            assert_eq!((c.code.as_ref(), c.exit), ("no_session", 2));
        }
        // A keyring that will not hand the session over is a sign-in: signing
        // in again rewrites the item under the service. It was listed below as
        // `engine_down` until that advice was found to hide the one fix.
        let c = classify(&anyhow::Error::new(IpcError::EngineDown {
            reason: "the keyring did not answer".into(),
            kind: hoard_core::ipc::EngineDownReason::KeyringUnreadable,
        }));
        assert_eq!((c.code.as_ref(), c.exit), ("no_session", 2));
        // Starting, stopping or failing: not a sign-in.
        for kind in [
            hoard_core::ipc::EngineDownReason::Unknown,
            hoard_core::ipc::EngineDownReason::Other,
        ] {
            let c = classify(&anyhow::Error::new(IpcError::EngineDown {
                reason: "the engine is still starting".into(),
                kind,
            }));
            assert_eq!((c.code.as_ref(), c.exit), ("engine_down", 1), "{kind:?}");
        }
        // A service older than `kind`: no session is still named in its text,
        // whatever the case; any other text stays `engine_down`.
        for (reason, code, exit) in [
            ("No session. Sign in with `hoard login`", "no_session", 2),
            ("the engine is still starting", "engine_down", 1),
        ] {
            let c = classify(&anyhow::Error::new(IpcError::EngineDown {
                reason: reason.into(),
                kind: hoard_core::ipc::EngineDownReason::Unknown,
            }));
            assert_eq!((c.code.as_ref(), c.exit), (code, exit), "{reason}");
        }
        // The fallback is for `Unknown` only: a classified reason wins over
        // the words in its text.
        let c = classify(&anyhow::Error::new(IpcError::EngineDown {
            reason: "no session yet, but the daemon is shutting down".into(),
            kind: hoard_core::ipc::EngineDownReason::Other,
        }));
        assert_eq!((c.code.as_ref(), c.exit), ("engine_down", 1));
    }

    #[test]
    fn truncate_marks_what_it_cut() {
        assert_eq!(truncate("raid", 4), "raid");
        assert_eq!(truncate("friends", 4), "fri…");
        assert_eq!(truncate("días", 3), "dí…");
    }

    /// Context added with `.context(…)` must not hide the typed cause.
    #[test]
    fn context_does_not_lose_the_code() {
        use anyhow::Context;
        let e = Err::<(), _>(ApiError::NotFound)
            .context("while fetching the save")
            .unwrap_err();
        assert_eq!(classify(&e).code, "not_found");
    }
}
