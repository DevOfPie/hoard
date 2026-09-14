//! Hosting leases on shared saves (`/v1/saves/{id}/lease`): acquire, keep
//! alive, give back.
//!
//! One task beside the presence beat, in the same shape: the engine sends it
//! commands through a [`LeaseHandle`], it talks to the server, and every
//! answer goes back to the engine as `AgentHandle::lease_verdict` (an
//! acquire's answer) or `AgentHandle::set_lease` (anything else), the one
//! place a slot's lease changes. The task holds no policy: whether to ask for a
//! lease is the reducer's call (`HOLD_LEASE_NEEDED`), and what a lost lease
//! means for a session is the engine's.
//!
//! - A held lease is renewed every [`RENEW_SECS`]; the server's TTL is 300 s,
//!   so a machine that dies stops holding on its own.
//! - A renew refused (`409 not_holder`: forced by a member, or expired while
//!   the network was down) drops the lease from the map and tells the engine
//!   who holds it now, which is where `WorldLeaseLost` comes from.
//! - A transport error changes nothing: the last state stands and the next tick
//!   retries. A release or a cancel drops that retry, so it cannot take back a
//!   lease the engine no longer wants.
//! - `closing()` releases every held lease, bounded, so quitting never hangs.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use hoard_core::kernel::LeaseObs;
use hoard_core::wire::Lease;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{interval, Instant, MissedTickBehavior};

use crate::agent::AgentHandle;
use crate::api::{ApiClient, ApiError};

/// Renew cadence (HRD-D-0003). The server's TTL is 300 s, so this survives
/// several missed beats.
pub const RENEW_SECS: u64 = 30;

/// The longest `closing()` waits for its releases before giving up.
const CLOSING_TIMEOUT_SECS: u64 = 3;

enum Cmd {
    Acquire { save_id: String, base_version: i64 },
    Release { save_id: String },
    Cancel { save_id: String },
    Force { save_id: String },
    Refresh { save_id: String },
    Closing { done: oneshot::Sender<()> },
}

/// A cheap handle to clone. The engine calls it from its command loop.
#[derive(Clone, Debug)]
pub struct LeaseHandle {
    tx: mpsc::Sender<Cmd>,
}

impl LeaseHandle {
    /// Ask for the lease with the head this machine has. `try_send` on
    /// purpose: a full channel loses a request rather than ever blocking the
    /// engine's loop; the reducer asks again on its next hold.
    pub fn acquire(&self, save_id: impl Into<String>, base_version: i64) {
        let _ = self.tx.try_send(Cmd::Acquire {
            save_id: save_id.into(),
            base_version,
        });
    }

    pub fn release(&self, save_id: impl Into<String>) {
        let _ = self.tx.try_send(Cmd::Release {
            save_id: save_id.into(),
        });
    }

    /// Stop asking for the lease: an acquire the task still retries is dropped
    /// and answers nothing. One already on the wire answers as usual, and the
    /// engine gives back a `Mine` it no longer wants (`claim::on_lease`).
    pub fn cancel(&self, save_id: impl Into<String>) {
        let _ = self.tx.try_send(Cmd::Cancel {
            save_id: save_id.into(),
        });
    }

    /// Take the lease off its holder. The caller acquires afterwards.
    pub fn force(&self, save_id: impl Into<String>) {
        let _ = self.tx.try_send(Cmd::Force {
            save_id: save_id.into(),
        });
    }

    /// Ask the server who holds the lease now. The answer arrives as
    /// `set_lease`, like every other; a slot still reading `Unknown` at
    /// launch asks this before the minute of grace runs.
    pub fn refresh(&self, save_id: impl Into<String>) {
        let _ = self.tx.try_send(Cmd::Refresh {
            save_id: save_id.into(),
        });
    }

    /// A handle whose commands land in a channel instead of a server, as
    /// `"<verb> <save_id>"` lines, for the engine tests.
    #[cfg(test)]
    pub(crate) fn probe() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (tx, mut rx) = mpsc::channel(64);
        let (seen_tx, seen_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let line = match cmd {
                    Cmd::Acquire { save_id, .. } => format!("acquire {save_id}"),
                    Cmd::Release { save_id } => format!("release {save_id}"),
                    Cmd::Cancel { save_id } => format!("cancel {save_id}"),
                    Cmd::Force { save_id } => format!("force {save_id}"),
                    Cmd::Refresh { save_id } => format!("refresh {save_id}"),
                    Cmd::Closing { done } => {
                        let _ = done.send(());
                        break;
                    }
                };
                let _ = seen_tx.send(line);
            }
        });
        (Self { tx }, seen_rx)
    }

    /// Release every held lease on an orderly shutdown. Bounded
    /// ([`CLOSING_TIMEOUT_SECS`]), so the quit path can call it without any risk
    /// of hanging.
    pub async fn closing(&self) {
        let (done_tx, done_rx) = oneshot::channel();
        if self.tx.send(Cmd::Closing { done: done_tx }).await.is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(CLOSING_TIMEOUT_SECS), done_rx).await;
        }
    }
}

/// Starts the lease task on the agent's own `ApiClient`. The task dies on its
/// own once every handle is dropped, or right after `closing()`.
pub fn spawn(api: ApiClient, agent: AgentHandle) -> (LeaseHandle, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(run(api, agent, rx));
    (LeaseHandle { tx }, task)
}

/// The five calls the task makes. A trait so the state machine is tested
/// against a scripted server rather than a socket.
trait LeaseApi: Send + Sync + 'static {
    fn acquire(
        &self,
        save_id: String,
        base_version: i64,
    ) -> impl Future<Output = anyhow::Result<Lease>> + Send;
    fn renew(&self, save_id: String) -> impl Future<Output = anyhow::Result<Lease>> + Send;
    fn release(&self, save_id: String) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn force(&self, save_id: String) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn current(
        &self,
        save_id: String,
    ) -> impl Future<Output = anyhow::Result<Option<Lease>>> + Send;
    /// This account\'s id, to tell a lease of ours from anybody else\'s.
    fn me(&self) -> impl Future<Output = anyhow::Result<String>> + Send;
}

impl LeaseApi for ApiClient {
    async fn acquire(&self, save_id: String, base_version: i64) -> anyhow::Result<Lease> {
        self.acquire_lease(&save_id, base_version).await
    }
    async fn renew(&self, save_id: String) -> anyhow::Result<Lease> {
        self.renew_lease(&save_id).await
    }
    async fn release(&self, save_id: String) -> anyhow::Result<()> {
        self.release_lease(&save_id).await
    }
    async fn force(&self, save_id: String) -> anyhow::Result<()> {
        self.force_lease(&save_id).await
    }
    async fn current(&self, save_id: String) -> anyhow::Result<Option<Lease>> {
        self.get_lease(&save_id).await
    }
    async fn me(&self) -> anyhow::Result<String> {
        Ok(self.whoami().await?.user_id)
    }
}

/// Where the answers go. The engine in production; a recorder in the tests.
/// `verdict` marks the answer to an acquire; everything else is an observation
/// and leaves the engine's pending request alone.
trait LeaseSink: Send + Sync + 'static {
    fn set_lease(
        &self,
        save_id: String,
        lease: LeaseObs,
        holder: Option<String>,
        verdict: bool,
    ) -> impl Future<Output = ()> + Send;
}

impl LeaseSink for AgentHandle {
    async fn set_lease(
        &self,
        save_id: String,
        lease: LeaseObs,
        holder: Option<String>,
        verdict: bool,
    ) {
        let sent = if verdict {
            AgentHandle::lease_verdict(self, save_id, lease, holder).await
        } else {
            AgentHandle::set_lease(self, save_id, lease, holder).await
        };
        if let Err(e) = sent {
            tracing::debug!(error = %e, "lease: the engine is gone");
        }
    }
}

/// A lease this machine holds.
struct Held {
    since: Instant,
}

/// What a refused call says about the lease: `Some(holder)` when the server
/// named who has it, `Some(None)` when it only said "not yours", `None` when
/// the call never reached a verdict (transport) and the last state stands.
fn refusal(err: &anyhow::Error) -> Option<Option<String>> {
    match err.downcast_ref::<ApiError>() {
        Some(ApiError::LeaseHeld(c)) | Some(ApiError::LeaseRequired(c)) => {
            Some(c.holder().map(String::from))
        }
        Some(ApiError::Conflict(_))
        | Some(ApiError::NotShared)
        | Some(ApiError::NotFound)
        | Some(ApiError::Forbidden) => Some(None),
        _ => None,
    }
}

async fn run<A: LeaseApi, S: LeaseSink>(api: A, sink: S, mut rx: mpsc::Receiver<Cmd>) {
    let mut held: HashMap<String, Held> = HashMap::new();
    // Who this account is, asked once, when a refresh first needs to tell
    // a lease of ours from anybody else\'s.
    let mut me: Option<String> = None;
    // Acquires the server never answered (transport), retried on the tick:
    // the engine asks once and waits for a verdict, so the retry is ours.
    let mut wanted: HashMap<String, i64> = HashMap::new();
    // The base each save was refused as stale with. The engine asks again on
    // every hold with the same head until a pull moves it; the server is not.
    let mut stale: HashMap<String, i64> = HashMap::new();

    let mut tick = interval(Duration::from_secs(RENEW_SECS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None => break,
                Some(Cmd::Acquire { save_id, base_version }) => {
                    acquire(&api, &sink, &mut held, &mut wanted, &mut stale, save_id, base_version).await;
                }
                Some(Cmd::Release { save_id }) => {
                    // A retry still queued would take back what is given back.
                    wanted.remove(&save_id);
                    match api.release(save_id.clone()).await {
                        Ok(()) => {}
                        Err(e) if refusal(&e).is_some() => {
                            tracing::debug!(save_id = %save_id, error = %e, "lease: release refused; not ours any more");
                        }
                        Err(e) => {
                            tracing::debug!(save_id = %save_id, error = %e, "lease: release failed (kept)");
                            continue;
                        }
                    }
                    held.remove(&save_id);
                    sink.set_lease(save_id, LeaseObs::Free, None, false).await;
                }
                Some(Cmd::Cancel { save_id }) => {
                    wanted.remove(&save_id);
                }
                Some(Cmd::Force { save_id }) => {
                    match api.force(save_id.clone()).await {
                        Ok(()) => {
                            tracing::info!(save_id = %save_id, "lease: forced off its holder");
                            sink.set_lease(save_id, LeaseObs::Free, None, false).await;
                        }
                        Err(e) => {
                            tracing::info!(save_id = %save_id, error = %e, "lease: force refused");
                        }
                    }
                }
                Some(Cmd::Refresh { save_id }) => {
                    // Only a verdict changes the slot: a transport error keeps
                    // the last state, like everywhere else in this task.
                    if me.is_none() {
                        me = api.me().await.ok();
                    }
                    match api.current(save_id.clone()).await {
                        Ok(Some(l)) => {
                            // Ours only when the server names this account; a
                            // takeover the renew has not seen yet reads as theirs.
                            let ours = match &me {
                                Some(id) => *id == l.holder_user_id,
                                None => held.contains_key(&save_id),
                            };
                            let holder = l.holder_username.as_str().to_string();
                            if ours {
                                // Ours on the server is ours to renew: a lease
                                // left by a restart expires otherwise.
                                held.entry(save_id.clone()).or_insert(Held {
                                    since: Instant::now(),
                                });
                                sink.set_lease(save_id, LeaseObs::Mine, Some(holder), false).await;
                            } else {
                                held.remove(&save_id);
                                sink.set_lease(save_id, LeaseObs::Other, Some(holder), false).await;
                            }
                        }
                        Ok(None) => sink.set_lease(save_id, LeaseObs::Free, None, false).await,
                        Err(e) => {
                            tracing::debug!(save_id = %save_id, error = %e, "lease: refresh failed (state kept)");
                        }
                    }
                }
                Some(Cmd::Closing { done }) => {
                    let ids: Vec<String> = held.drain().map(|(id, _)| id).collect();
                    let releases = async {
                        for id in ids {
                            let _ = api.release(id).await;
                        }
                    };
                    let _ = tokio::time::timeout(
                        Duration::from_secs(CLOSING_TIMEOUT_SECS),
                        releases,
                    )
                    .await;
                    let _ = done.send(());
                    break;
                }
            },
            _ = tick.tick() => {
                for (save_id, base) in wanted.clone() {
                    acquire(&api, &sink, &mut held, &mut wanted, &mut stale, save_id, base).await;
                }
                renew_all(&api, &sink, &mut held).await;
            }
        }
    }
}

async fn acquire<A: LeaseApi, S: LeaseSink>(
    api: &A,
    sink: &S,
    held: &mut HashMap<String, Held>,
    wanted: &mut HashMap<String, i64>,
    stale: &mut HashMap<String, i64>,
    save_id: String,
    base_version: i64,
) {
    if stale.get(&save_id) == Some(&base_version) {
        // Refused as stale with this very head: asking again returns the same
        // answer. Nothing is reported, so the engine waits until its head moves.
        tracing::debug!(save_id = %save_id, base_version, "lease: still behind the head; not asking again");
        return;
    }
    match api.acquire(save_id.clone(), base_version).await {
        Ok(lease) => {
            wanted.remove(&save_id);
            stale.remove(&save_id);
            held.entry(save_id.clone()).or_insert(Held {
                since: Instant::now(),
            });
            let me = lease.holder_username.as_str().to_string();
            sink.set_lease(save_id, LeaseObs::Mine, Some(me), true)
                .await;
        }
        Err(e) => match e.downcast_ref::<ApiError>() {
            Some(ApiError::LeaseHeld(c)) => {
                let holder = c.holder().map(String::from);
                tracing::info!(save_id = %save_id, holder = holder.as_deref().unwrap_or("?"), "lease: held by another member");
                wanted.remove(&save_id);
                held.remove(&save_id);
                sink.set_lease(save_id, LeaseObs::Other, holder, true).await;
            }
            Some(ApiError::LeaseStale(st)) => {
                // Nobody holds it; this machine is behind. The pull is the
                // reducer's business, and it asks again once its head moved.
                tracing::info!(save_id = %save_id, head = ?st.head(), "lease: refused, pull first");
                wanted.remove(&save_id);
                held.remove(&save_id);
                stale.insert(save_id.clone(), base_version);
                sink.set_lease(save_id, LeaseObs::Free, None, true).await;
            }
            _ if refusal(&e).is_some() => {
                // A verdict with nothing to hold: not shared, not found, not
                // ours to ask. The engine's request stands unanswered on purpose.
                tracing::info!(save_id = %save_id, error = %e, "lease: acquire refused");
                wanted.remove(&save_id);
            }
            _ => {
                tracing::debug!(save_id = %save_id, error = %e, "lease: acquire failed; retrying on the tick");
                wanted.insert(save_id, base_version);
            }
        },
    }
}

async fn renew_all<A: LeaseApi, S: LeaseSink>(api: &A, sink: &S, held: &mut HashMap<String, Held>) {
    let ids: Vec<String> = held.keys().cloned().collect();
    for save_id in ids {
        let err = match api.renew(save_id.clone()).await {
            Ok(_) => continue,
            Err(e) => e,
        };
        let Some(named) = refusal(&err) else {
            tracing::debug!(save_id = %save_id, error = %err, "lease: renew failed (retry on next tick)");
            continue;
        };
        let held_secs = held
            .remove(&save_id)
            .map_or(0, |h| h.since.elapsed().as_secs());
        // `not_holder` carries no lease, so who has it now is a second question.
        let holder = match named {
            Some(h) => Some(h),
            None => api
                .current(save_id.clone())
                .await
                .ok()
                .flatten()
                .map(|l| l.holder_username.as_str().to_string()),
        };
        tracing::warn!(save_id = %save_id, held_secs, holder = holder.as_deref().unwrap_or("nobody"), "lease: lost");
        let obs = if holder.is_some() {
            LeaseObs::Other
        } else {
            LeaseObs::Free
        };
        sink.set_lease(save_id, obs, holder, false).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use time::OffsetDateTime;

    fn lease(holder: &str) -> Lease {
        Lease {
            save_id: "w1".into(),
            holder_user_id: format!("id-{holder}"),
            holder_username: hoard_core::ids::Username::parse(holder).unwrap(),
            holder_device_fp: None,
            acquired_at: OffsetDateTime::UNIX_EPOCH,
            renewed_at: OffsetDateTime::UNIX_EPOCH,
            base_version: 1,
            pushed_since: false,
            live: true,
        }
    }

    fn held_by(holder: &str) -> anyhow::Error {
        ApiError::LeaseHeld(crate::api::LeaseConflict {
            code: "held".into(),
            error: String::new(),
            lease: Some(Box::new(lease(holder))),
        })
        .into()
    }

    fn not_holder() -> anyhow::Error {
        ApiError::Conflict("you do not hold a live lease on this save".into()).into()
    }

    fn transport() -> anyhow::Error {
        anyhow::anyhow!("connection refused")
    }
    fn stale(head: i64, base: i64) -> anyhow::Error {
        ApiError::LeaseStale(crate::api::LeaseStale {
            head_version: head,
            base_version: base,
        })
        .into()
    }

    /// A scripted server: what each call answers, and what it was asked.
    #[derive(Default)]
    struct Script {
        acquire: Mutex<Vec<anyhow::Result<Lease>>>,
        renew: Mutex<Vec<anyhow::Result<Lease>>>,
        current: Mutex<Option<Lease>>,
        released: Mutex<Vec<String>>,
        forced: Mutex<Vec<String>>,
        renews: Mutex<u32>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Script>);

    impl LeaseApi for Fake {
        fn acquire(
            &self,
            _save_id: String,
            _base_version: i64,
        ) -> impl Future<Output = anyhow::Result<Lease>> + Send {
            let r = self.0.acquire.lock().unwrap().remove(0);
            async move { r }
        }
        fn renew(&self, _save_id: String) -> impl Future<Output = anyhow::Result<Lease>> + Send {
            *self.0.renews.lock().unwrap() += 1;
            let mut q = self.0.renew.lock().unwrap();
            let r = if q.is_empty() {
                Ok(lease("me"))
            } else {
                q.remove(0)
            };
            async move { r }
        }
        fn release(&self, save_id: String) -> impl Future<Output = anyhow::Result<()>> + Send {
            self.0.released.lock().unwrap().push(save_id);
            async { Ok(()) }
        }
        fn force(&self, save_id: String) -> impl Future<Output = anyhow::Result<()>> + Send {
            self.0.forced.lock().unwrap().push(save_id);
            async { Ok(()) }
        }
        fn current(
            &self,
            _save_id: String,
        ) -> impl Future<Output = anyhow::Result<Option<Lease>>> + Send {
            let r = self.0.current.lock().unwrap().clone();
            async move { Ok(r) }
        }
        async fn me(&self) -> anyhow::Result<String> {
            Ok("id-me".to_string())
        }
    }

    type Seen = Vec<(String, LeaseObs, Option<String>)>;

    /// What the engine was told, and beside it whether each was a verdict.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Seen>>, Arc<Mutex<Vec<bool>>>);

    impl LeaseSink for Recorder {
        fn set_lease(
            &self,
            save_id: String,
            lease: LeaseObs,
            holder: Option<String>,
            verdict: bool,
        ) -> impl Future<Output = ()> + Send {
            self.0.lock().unwrap().push((save_id, lease, holder));
            self.1.lock().unwrap().push(verdict);
            async {}
        }
    }

    fn start(fake: Fake) -> (LeaseHandle, Recorder, JoinHandle<()>) {
        let sink = Recorder::default();
        let (tx, rx) = mpsc::channel(8);
        let task = tokio::spawn(run(fake, sink.clone(), rx));
        (LeaseHandle { tx }, sink, task)
    }

    /// Lets the task drain its channel. Under the paused clock a sleep advances
    /// time only once every task is idle.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_acquired_lease_is_mine_and_gets_renewed() {
        let fake = Fake::default();
        fake.0.acquire.lock().unwrap().push(Ok(lease("me")));
        let (h, sink, _task) = start(fake.clone());

        h.acquire("w1", 1);
        settle().await;
        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            &[("w1".to_string(), LeaseObs::Mine, Some("me".to_string()))]
        );

        // One tick per jump: a delayed interval fires once however far the
        // clock moves.
        for _ in 0..2 {
            tokio::time::advance(Duration::from_secs(RENEW_SECS + 1)).await;
            settle().await;
        }
        assert!(*fake.0.renews.lock().unwrap() >= 2, "renewed on the tick");
        // A good renew is quiet.
        assert_eq!(sink.0.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_refusal_names_the_holder_and_holds_nothing() {
        let fake = Fake::default();
        fake.0.acquire.lock().unwrap().push(Err(held_by("bob")));
        let (h, sink, _task) = start(fake.clone());

        h.acquire("w1", 1);
        settle().await;
        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            &[("w1".to_string(), LeaseObs::Other, Some("bob".to_string()))]
        );
        tokio::time::advance(Duration::from_secs(RENEW_SECS + 1)).await;
        settle().await;
        assert_eq!(*fake.0.renews.lock().unwrap(), 0, "nothing to renew");
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_renew_drops_the_lease_and_says_who_has_it() {
        let fake = Fake::default();
        fake.0.acquire.lock().unwrap().push(Ok(lease("me")));
        fake.0.renew.lock().unwrap().push(Err(not_holder()));
        *fake.0.current.lock().unwrap() = Some(lease("bob"));
        let (h, sink, _task) = start(fake.clone());

        h.acquire("w1", 1);
        settle().await;
        tokio::time::advance(Duration::from_secs(RENEW_SECS + 1)).await;
        settle().await;
        let seen = sink.0.lock().unwrap().clone();
        assert_eq!(
            seen.last(),
            Some(&("w1".to_string(), LeaseObs::Other, Some("bob".to_string())))
        );
        let renews = *fake.0.renews.lock().unwrap();
        tokio::time::advance(Duration::from_secs(RENEW_SECS + 1)).await;
        settle().await;
        assert_eq!(
            *fake.0.renews.lock().unwrap(),
            renews,
            "dropped from the map"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_renew_with_nobody_holding_reads_free() {
        let fake = Fake::default();
        fake.0.acquire.lock().unwrap().push(Ok(lease("me")));
        fake.0.renew.lock().unwrap().push(Err(not_holder()));
        let (h, sink, _task) = start(fake.clone());

        h.acquire("w1", 1);
        settle().await;
        tokio::time::advance(Duration::from_secs(RENEW_SECS + 1)).await;
        settle().await;
        assert_eq!(
            sink.0.lock().unwrap().last(),
            Some(&("w1".to_string(), LeaseObs::Free, None))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_transport_error_keeps_the_last_state() {
        let fake = Fake::default();
        fake.0.acquire.lock().unwrap().push(Err(transport()));
        fake.0.acquire.lock().unwrap().push(Ok(lease("me")));
        fake.0.renew.lock().unwrap().push(Err(transport()));
        let (h, sink, _task) = start(fake.clone());

        h.acquire("w1", 1);
        settle().await;
        // The failed acquire is the task's to retry on its tick; the engine is
        // told nothing until there is a verdict. One tick per jump: a delayed
        // interval fires once however far the clock moves.
        for _ in 0..2 {
            tokio::time::advance(Duration::from_secs(RENEW_SECS + 1)).await;
            settle().await;
        }
        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            &[("w1".to_string(), LeaseObs::Mine, Some("me".to_string()))],
            "one verdict; a failed renew is not a loss"
        );
        assert!(
            *fake.0.renews.lock().unwrap() >= 2,
            "still held, still renewed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn release_and_force_read_free() {
        let fake = Fake::default();
        fake.0.acquire.lock().unwrap().push(Ok(lease("me")));
        let (h, sink, _task) = start(fake.clone());

        h.acquire("w1", 1);
        h.release("w1");
        h.force("w2");
        settle().await;
        assert_eq!(
            fake.0.released.lock().unwrap().as_slice(),
            &["w1".to_string()]
        );
        assert_eq!(
            fake.0.forced.lock().unwrap().as_slice(),
            &["w2".to_string()]
        );
        let seen = sink.0.lock().unwrap().clone();
        assert_eq!(seen[1], ("w1".to_string(), LeaseObs::Free, None));
        assert_eq!(seen[2], ("w2".to_string(), LeaseObs::Free, None));
    }

    /// A refresh reports what the server says and touches nothing else:
    /// somebody's lease reads as theirs, no lease reads free, and a held one
    /// stays ours.
    #[tokio::test(start_paused = true)]
    async fn a_refresh_reads_the_servers_answer() {
        let fake = Fake::default();
        *fake.0.current.lock().unwrap() = Some(lease("bob"));
        let (handle, seen, task) = start(fake.clone());
        handle.refresh("w1");
        settle().await;
        assert_eq!(
            seen.0.lock().unwrap().as_slice(),
            &[("w1".to_string(), LeaseObs::Other, Some("bob".to_string()))]
        );

        *fake.0.current.lock().unwrap() = None;
        handle.refresh("w1");
        settle().await;
        assert_eq!(
            seen.0.lock().unwrap().last().unwrap(),
            &("w1".to_string(), LeaseObs::Free, None)
        );

        fake.0.acquire.lock().unwrap().push(Ok(lease("me")));
        *fake.0.current.lock().unwrap() = Some(lease("me"));
        handle.acquire("w1", 1);
        handle.refresh("w1");
        settle().await;
        assert_eq!(seen.0.lock().unwrap().last().unwrap().1, LeaseObs::Mine);
        drop(handle);
        let _ = task.await;
    }

    #[tokio::test(start_paused = true)]
    async fn closing_releases_every_held_lease() {
        let fake = Fake::default();
        fake.0.acquire.lock().unwrap().push(Ok(lease("me")));
        fake.0.acquire.lock().unwrap().push(Ok(lease("me")));
        let (h, _sink, task) = start(fake.clone());

        h.acquire("w1", 1);
        h.acquire("w2", 1);
        settle().await;
        h.closing().await;
        let mut released = fake.0.released.lock().unwrap().clone();
        released.sort();
        assert_eq!(released, vec!["w1".to_string(), "w2".to_string()]);
        assert!(task.await.is_ok(), "the task ends after closing");
    }

    /// Refused as stale, the task asks the server once per head: the engine
    /// keeps asking on every hold until its head moves, the server is not.
    #[tokio::test(start_paused = true)]
    async fn a_stale_acquire_is_asked_once_per_head() {
        let fake = Fake::default();
        fake.0.acquire.lock().unwrap().push(Err(stale(2, 1)));
        fake.0.acquire.lock().unwrap().push(Ok(lease("me")));
        let (h, sink, _task) = start(fake.clone());
        h.acquire("w1", 1);
        settle().await;
        assert_eq!(
            sink.0.lock().unwrap().last(),
            Some(&("w1".to_string(), LeaseObs::Free, None))
        );
        h.acquire("w1", 1);
        settle().await;
        assert_eq!(
            fake.0.acquire.lock().unwrap().len(),
            1,
            "not asked again for the same head"
        );
        assert_eq!(sink.0.lock().unwrap().len(), 1, "nothing reported either");
        h.acquire("w1", 2);
        settle().await;
        assert_eq!(
            sink.0.lock().unwrap().last(),
            Some(&("w1".to_string(), LeaseObs::Mine, Some("me".to_string())))
        );
    }

    /// The server never answered: the task keeps the request and asks again on
    /// its tick, so a stumble at the first push does not silence the session.
    #[tokio::test(start_paused = true)]
    async fn an_unanswered_acquire_is_retried_on_the_tick() {
        let fake = Fake::default();
        fake.0.acquire.lock().unwrap().push(Err(transport()));
        fake.0.acquire.lock().unwrap().push(Ok(lease("me")));
        let (h, sink, _task) = start(fake.clone());
        h.acquire("w1", 1);
        settle().await;
        // The first answer was no verdict; the tick (immediate on a fresh
        // interval, then every RENEW_SECS) asks again and gets the lease.
        tokio::time::advance(Duration::from_secs(RENEW_SECS + 1)).await;
        settle().await;
        assert!(fake.0.acquire.lock().unwrap().is_empty(), "asked twice");
        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            &[("w1".to_string(), LeaseObs::Mine, Some("me".to_string()))],
            "one verdict, no noise for the failed try"
        );
    }

    /// A release, or a cancel, while an acquire the server never answered
    /// waits for the tick: the retry is dropped, and nothing ends held.
    #[tokio::test(start_paused = true)]
    async fn a_release_or_cancel_drops_the_acquire_waiting_for_the_tick() {
        for verb in ["release", "cancel"] {
            let fake = Fake::default();
            fake.0.acquire.lock().unwrap().push(Err(transport()));
            fake.0.acquire.lock().unwrap().push(Ok(lease("me")));
            let (h, sink, _task) = start(fake.clone());
            // A fresh interval ticks at once: spend it, so the retry waits
            // for the next one.
            settle().await;
            h.acquire("w1", 1);
            settle().await;
            match verb {
                "release" => h.release("w1"),
                _ => h.cancel("w1"),
            }
            settle().await;
            for _ in 0..2 {
                tokio::time::advance(Duration::from_secs(RENEW_SECS + 1)).await;
                settle().await;
            }
            assert_eq!(
                fake.0.acquire.lock().unwrap().len(),
                1,
                "{verb}: retried the acquire"
            );
            assert_eq!(*fake.0.renews.lock().unwrap(), 0, "{verb}: held");
            let seen = sink.0.lock().unwrap().clone();
            match verb {
                "release" => assert_eq!(seen, [("w1".to_string(), LeaseObs::Free, None)]),
                _ => assert!(seen.is_empty(), "{verb}: {seen:?}"),
            }
        }
    }

    /// A refresh that finds this account holding a lease the task did not
    /// know of (a restart within the TTL) keeps it alive, and tells the
    /// engine as an observation, not as the answer to an acquire.
    #[tokio::test(start_paused = true)]
    async fn a_refresh_that_reads_mine_is_renewed() {
        let fake = Fake::default();
        fake.0.acquire.lock().unwrap().push(Ok(lease("me")));
        *fake.0.current.lock().unwrap() = Some(lease("me"));
        let (h, sink, _task) = start(fake.clone());
        h.refresh("w1");
        settle().await;
        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            &[("w1".to_string(), LeaseObs::Mine, Some("me".to_string()))]
        );
        // The fresh interval's immediate tick may already have renewed it.
        let before = *fake.0.renews.lock().unwrap();
        tokio::time::advance(Duration::from_secs(RENEW_SECS + 1)).await;
        settle().await;
        assert!(
            *fake.0.renews.lock().unwrap() > before,
            "renewed on the next tick"
        );
        h.acquire("w1", 1);
        settle().await;
        assert_eq!(sink.1.lock().unwrap().as_slice(), &[false, true]);
    }

    /// A refresh reads mine only when the server names this account, whatever
    /// the task still thinks it holds.
    #[tokio::test(start_paused = true)]
    async fn a_refresh_names_mine_only_for_this_account() {
        let fake = Fake::default();
        fake.0.acquire.lock().unwrap().push(Ok(lease("me")));
        let (h, sink, _task) = start(fake.clone());
        h.acquire("w1", 1);
        settle().await;
        *fake.0.current.lock().unwrap() = Some(lease("bob"));
        h.refresh("w1");
        settle().await;
        assert_eq!(
            sink.0.lock().unwrap().last(),
            Some(&("w1".to_string(), LeaseObs::Other, Some("bob".to_string())))
        );
        *fake.0.current.lock().unwrap() = Some(lease("me"));
        h.refresh("w1");
        settle().await;
        assert_eq!(
            sink.0.lock().unwrap().last(),
            Some(&("w1".to_string(), LeaseObs::Mine, Some("me".to_string())))
        );
    }
}
