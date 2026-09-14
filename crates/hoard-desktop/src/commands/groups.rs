//! Groups, shares and world leases, as a client of the service.
//!
//! Every command is one IPC request to `hoardd`, the same road `backup_now` takes:
//! the daemon holds the session and talks to the server, and it owns the lease
//! rules. Nothing here decides anything; it forwards the verb and maps the
//! answer. The 409 codes the server uses (`held`, `stale`, `pushed`...) reach the
//! UI as i18n keys so the dialog reads as a sentence, not as a tag.

use hoard_core::ipc::{IpcError, Payload, Request, WorldFiles, WorldLease, WorldRole};
use hoard_core::wire::{Group, InviteOut, Lease, Save};
use tauri::State;

use super::error::AppError;
use crate::state::AppState;

/// The lease as the card draws it: the server's row, and whether this machine
/// is the holder. `here` is decided by the device fingerprint the service
/// stamps on a lease it takes, not by the user, so the same account on two
/// machines sees "hosted by" on the one that is not hosting.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LeaseView {
    pub lease: Option<Lease>,
    pub here: bool,
}

/// A 409 keeps its code on the wire; here it becomes the key of the sentence
/// the user reads. Unknown codes fall back to the server's own message.
fn conflict_key(code: &str) -> Option<&'static str> {
    Some(match code {
        "held" => "lease.err_held",
        "stale" => "lease.err_stale",
        "lease_required" => "lease.err_required",
        "not_shared" => "share.err_not_shared",
        "already_shared" => "share.err_already_shared",
        "lease_held" => "share.err_lease_held",
        "pushed" => "lease.err_pushed",
        "not_holder" => "lease.err_not_holder",
        "not_held" => "lease.err_not_held",
        _ => return None,
    })
}

fn map_err(err: anyhow::Error) -> AppError {
    match err.downcast_ref::<IpcError>() {
        Some(IpcError::Conflict { code, message }) => match conflict_key(code) {
            Some(key) => AppError::new("groups.err_title", key).with_detail(message.clone()),
            None => AppError::new("groups.err_title", message.clone()),
        },
        Some(IpcError::Invalid { message }) => AppError::new("groups.err_title", message.clone()),
        Some(IpcError::EngineDown { reason, .. }) => {
            AppError::new("groups.err_title", "groups.err_engine_down").with_detail(reason.clone())
        }
        _ => {
            AppError::new("groups.err_title", "groups.err_generic").with_detail(format!("{err:#}"))
        }
    }
}

async fn ask(state: &State<'_, AppState>, request: Request) -> Result<Payload, AppError> {
    state.daemon.request(request).await.map_err(map_err)
}

fn unexpected(what: &str, other: Payload) -> AppError {
    AppError::new("groups.err_title", "groups.err_generic")
        .with_detail(format!("unexpected answer to {what}: {other:?}"))
}

#[tauri::command]
pub async fn list_groups(state: State<'_, AppState>) -> Result<Vec<Group>, AppError> {
    match ask(&state, Request::ListGroups).await? {
        Payload::Groups { groups } => Ok(groups),
        other => Err(unexpected("list_groups", other)),
    }
}

#[tauri::command]
pub async fn create_group(name: String, state: State<'_, AppState>) -> Result<Group, AppError> {
    match ask(&state, Request::CreateGroup { name }).await? {
        Payload::Group(group) => Ok(*group),
        other => Err(unexpected("create_group", other)),
    }
}

#[tauri::command]
pub async fn invite_to_group(
    group_id: String,
    expires_in_secs: Option<u64>,
    state: State<'_, AppState>,
) -> Result<InviteOut, AppError> {
    match ask(
        &state,
        Request::InviteToGroup {
            group_id,
            expires_in_secs,
        },
    )
    .await?
    {
        Payload::Invite(invite) => Ok(invite),
        other => Err(unexpected("invite_to_group", other)),
    }
}

#[tauri::command]
pub async fn join_group(token: String, state: State<'_, AppState>) -> Result<Group, AppError> {
    match ask(&state, Request::JoinGroup { token }).await? {
        Payload::Group(group) => Ok(*group),
        other => Err(unexpected("join_group", other)),
    }
}

#[tauri::command]
pub async fn leave_group(group_id: String, state: State<'_, AppState>) -> Result<(), AppError> {
    ask(&state, Request::LeaveGroup { group_id })
        .await
        .map(|_| ())
}

#[tauri::command]
pub async fn remove_member(
    group_id: String,
    user_id: String,
    state: State<'_, AppState>,
) -> Result<(), AppError> {
    ask(&state, Request::RemoveMember { group_id, user_id })
        .await
        .map(|_| ())
}

#[tauri::command]
pub async fn delete_group(group_id: String, state: State<'_, AppState>) -> Result<(), AppError> {
    ask(&state, Request::DeleteGroup { group_id })
        .await
        .map(|_| ())
}

#[tauri::command]
pub async fn share_save(
    save_id: String,
    group_id: String,
    world: Option<String>,
    state: State<'_, AppState>,
) -> Result<Save, AppError> {
    match ask(
        &state,
        Request::ShareSave {
            save_id,
            group_id,
            world,
        },
    )
    .await?
    {
        Payload::Save(save) => Ok(*save),
        other => Err(unexpected("share_save", other)),
    }
}

#[tauri::command]
pub async fn unshare_save(save_id: String, state: State<'_, AppState>) -> Result<(), AppError> {
    ask(&state, Request::UnshareSave { save_id })
        .await
        .map(|_| ())
}

/// Accepted at once; the outcome arrives as `agent://world-claimed` or
/// `agent://world-hosted-elsewhere`.
#[tauri::command]
pub async fn claim_world(
    save_id: String,
    role: WorldRole,
    state: State<'_, AppState>,
) -> Result<(), AppError> {
    ask(&state, Request::ClaimWorld { save_id, role })
        .await
        .map(|_| ())
}

#[tauri::command]
pub async fn release_world(save_id: String, state: State<'_, AppState>) -> Result<(), AppError> {
    ask(&state, Request::ReleaseWorld { save_id })
        .await
        .map(|_| ())
}

#[tauri::command]
pub async fn force_world(save_id: String, state: State<'_, AppState>) -> Result<(), AppError> {
    ask(&state, Request::ForceWorld { save_id })
        .await
        .map(|_| ())
}

#[tauri::command]
pub async fn dismiss_world(save_id: String, state: State<'_, AppState>) -> Result<(), AppError> {
    ask(&state, Request::DismissWorld { save_id })
        .await
        .map(|_| ())
}

#[tauri::command]
pub async fn get_lease(save_id: String, state: State<'_, AppState>) -> Result<LeaseView, AppError> {
    match ask(
        &state,
        Request::GetLease {
            save_id: save_id.clone(),
        },
    )
    .await?
    {
        Payload::Lease(lease) => {
            let lease = lease.map(|l| *l);
            // The engine decides by account once it knows who holds the lease;
            // while its slot reads unknown or free, or it has none, the
            // fingerprint on the lease says.
            let engine = match ask(&state, Request::Status).await {
                Ok(Payload::Status(status)) => status
                    .slots
                    .iter()
                    .find(|s| s.save_id == save_id)
                    .and_then(|s| s.lease),
                _ => None,
            };
            let here = lease.as_ref().is_some_and(|l| match engine {
                Some(WorldLease::Mine) => true,
                Some(WorldLease::Other) => false,
                _ => {
                    l.holder_device_fp.as_deref()
                        == Some(hoard_agent::logship::device_identity().fingerprint.as_str())
                }
            });
            Ok(LeaseView { lease, here })
        }
        other => Err(unexpected("get_lease", other)),
    }
}

/// The worlds a tracked save holds, with what a share of each would carry.
/// Empty for a game that shares whole.
#[tauri::command]
pub async fn list_worlds(
    save_id: String,
    state: State<'_, AppState>,
) -> Result<Vec<WorldFiles>, AppError> {
    match ask(&state, Request::ListWorlds { save_id }).await? {
        Payload::Worlds { worlds } => Ok(worlds),
        other => Err(unexpected("list_worlds", other)),
    }
}
