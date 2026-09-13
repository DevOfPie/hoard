//! Hosting leases on shared saves (`/v1/saves/{id}/lease`): acquire, keep
//! alive, give back.
//!
//! One task beside the presence beat, in the same shape: the engine sends it
//! commands through a [`LeaseHandle`], it talks to the server, and every
//! answer goes back to the engine as `AgentHandle::set_lease`, which is the one
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
//!   retries.
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
    Force { save_id: String },
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

    /// Take the lease off its holder. The caller acquires afterwards.
    pub fn force(&self, save_id: impl Into<String>) {
        let _ = self.tx.try_send(Cmd::Force {
            save_id: save_id.into(),
        });
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
}

/// Where the answers go. The engine in production; a recorder in the tests.
trait LeaseSink: Send + Sync + 'static {
    fn set_lease(
        &self,
        save_id: String,
        lease: LeaseObs,
        holder: Option<String>,
    ) -> impl Future<Output = ()> + Send;
}

impl LeaseSink for AgentHandle {
    async fn set_lease(&self, save_id: String, lease: LeaseObs, holder: Option<String>) {
        if let Err(e) = AgentHandle::set_lease(self, save_id, lease, holder).await {
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

    let mut tick = interval(Duration::from_secs(RENEW_SECS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None => break,
                Some(Cmd::Acquire { save_id, base_version }) => {
                    acquire(&api, &sink, &mut held, save_id, base_version).await;
                }
                Some(Cmd::Release { save_id }) => {
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
                    sink.set_lease(save_id, LeaseObs::Free, None).await;
                }
                Some(Cmd::Force { save_id }) => {
                    match api.force(save_id.clone()).await {
                        Ok(()) => {
                            tracing::info!(save_id = %save_id, "lease: forced off its holder");
                            sink.set_lease(save_id, LeaseObs::Free, None).await;
                        }
                        Err(e) => {
                            tracing::info!(save_id = %save_id, error = %e, "lease: force refused");
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
                renew_all(&api, &sink, &mut held).await;
            }
        }
    }
}

async fn acquire<A: LeaseApi, S: LeaseSink>(
    api: &A,
    sink: &S,
    held: &mut HashMap<String, Held>,
    save_id: String,
    base_version: i64,
) {
    match api.acquire(save_id.clone(), base_version).await {
        Ok(lease) => {
            held.entry(save_id.clone()).or_insert(Held {
                since: Instant::now(),
            });
            let me = lease.holder_username.as_str().to_string();
            sink.set_lease(save_id, LeaseObs::Mine, Some(me)).await;
        }
        Err(e) => match e.downcast_ref::<ApiError>() {
            Some(ApiError::LeaseHeld(c)) => {
                let holder = c.holder().map(String::from);
                tracing::info!(save_id = %save_id, holder = holder.as_deref().unwrap_or("?"), "lease: held by another member");
                held.remove(&save_id);
                sink.set_lease(save_id, LeaseObs::Other, holder).await;
            }
            Some(ApiError::LeaseStale(st)) => {
                // Nobody holds it; this machine is behind. The pull is the
                // reducer's business, and the next hold asks again.
                tracing::info!(save_id = %save_id, head = ?st.head(), "lease: refused, pull first");
                held.remove(&save_id);
                sink.set_lease(save_id, LeaseObs::Free, None).await;
            }
            _ => {
                tracing::debug!(save_id = %save_id, error = %e, "lease: acquire failed (state kept)");
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
        sink.set_lease(save_id, obs, holder).await;
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
    }

    type Seen = Vec<(String, LeaseObs, Option<String>)>;

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Seen>>);

    impl LeaseSink for Recorder {
        fn set_lease(
            &self,
            save_id: String,
            lease: LeaseObs,
            holder: Option<String>,
        ) -> impl Future<Output = ()> + Send {
            self.0.lock().unwrap().push((save_id, lease, holder));
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
        assert!(sink.0.lock().unwrap().is_empty(), "no verdict, no update");

        h.acquire("w1", 1);
        settle().await;
        // One tick per jump: a delayed interval fires once however far the
        // clock moves.
        for _ in 0..2 {
            tokio::time::advance(Duration::from_secs(RENEW_SECS + 1)).await;
            settle().await;
        }
        assert_eq!(
            sink.0.lock().unwrap().len(),
            1,
            "a failed renew is not a loss"
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
}
