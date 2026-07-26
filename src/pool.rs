//! Browser instance pool with rolling recycle.
//!
//! Replaces the original single shared `Browser` with a fixed-size pool of
//! `pool_size` chromium processes. It buys three things over one instance:
//!
//! * **Blast radius** — a crashing chromium takes out only its own in-flight
//!   requests; the other instances keep serving.
//! * **Bounded memory** — each instance is *recycled* (drained, then its
//!   subprocess replaced) after it has served `recycle_after_requests`
//!   requests or reached `recycle_after` age, so long-lived chromium memory
//!   creep can't grow without bound.
//! * **Zero-downtime recycle** — at most one instance is unavailable at a
//!   time (`recycle_token` has 1 permit), and a voluntarily-recycled instance
//!   drains its in-flight requests before its subprocess is swapped, so with
//!   `pool_size >= 2` callers never observe a recycle.
//!
//! Concurrency is bounded per instance by an owned semaphore of
//! `pages_per_instance` permits; total concurrency is
//! `pool_size * pages_per_instance`. [`BrowserPool::checkout`] routes each
//! request to the least-loaded active instance.
//!
//! Defaults are backwards-compatible: `pool_size = 1` and recycling disabled
//! reproduce the original single-instance behaviour exactly.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use chromiumoxide::Browser;
use tokio::sync::{Notify, OwnedSemaphorePermit, RwLock, Semaphore, oneshot};

use crate::browser;

const M_POOL_SIZE: &str = "browser_headless_pool_size";
const M_ACTIVE_INSTANCES: &str = "browser_headless_pool_active_instances";
const M_RESPAWNS: &str = "browser_headless_browser_respawns_total";
const M_RECYCLES: &str = "browser_headless_recycles_total";

/// At most one instance may be unavailable (draining / respawning) at a time,
/// so a recycle or crash never drops pool capacity by more than one instance.
const MAX_UNAVAILABLE: usize = 1;

/// Poll interval while waiting for an instance's in-flight requests to drain.
const DRAIN_POLL: Duration = Duration::from_millis(50);

/// How many times `checkout` re-picks when its chosen instance races into a
/// non-active state between selection and permit acquisition.
const CHECKOUT_PICK_RETRIES: usize = 4;

/// Pool sizing + recycle policy, resolved once from the environment.
#[derive(Clone)]
pub struct PoolConfig {
    /// Number of chromium processes (`BROWSER_HEADLESS_POOL_SIZE`, default 1).
    pub pool_size: usize,
    /// Page-concurrency cap per instance (`BROWSER_HEADLESS_MAX_PAGES`,
    /// default 8). Total concurrency = `pool_size * pages_per_instance`.
    pub pages_per_instance: usize,
    /// Recycle an instance after it has served this many requests. `0`
    /// disables count-based recycling (`BROWSER_HEADLESS_RECYCLE_AFTER_REQUESTS`).
    pub recycle_after_requests: u64,
    /// Recycle an instance once it reaches this age. `None` disables
    /// age-based recycling (`BROWSER_HEADLESS_RECYCLE_AFTER_SECS`).
    pub recycle_after: Option<Duration>,
    /// How long a voluntary recycle waits for in-flight requests to finish
    /// before swapping the subprocess anyway
    /// (`BROWSER_HEADLESS_DRAIN_TIMEOUT_MS`, default 30000).
    pub drain_timeout: Duration,
}

impl PoolConfig {
    pub fn from_env() -> Self {
        let recycle_after_secs = env_u64("BROWSER_HEADLESS_RECYCLE_AFTER_SECS", 0);
        Self {
            pool_size: env_usize("BROWSER_HEADLESS_POOL_SIZE", 1).max(1),
            pages_per_instance: env_usize("BROWSER_HEADLESS_MAX_PAGES", 8).max(1),
            recycle_after_requests: env_u64("BROWSER_HEADLESS_RECYCLE_AFTER_REQUESTS", 0),
            recycle_after: (recycle_after_secs > 0)
                .then(|| Duration::from_secs(recycle_after_secs)),
            drain_timeout: Duration::from_millis(
                env_u64("BROWSER_HEADLESS_DRAIN_TIMEOUT_MS", 30_000).max(1),
            ),
        }
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Routing state for one instance. Only `Active` instances receive new
/// checkouts; `Draining` (voluntary recycle) and `Down` (crashed / mid-
/// relaunch) are skipped.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Status {
    Active = 0,
    Draining = 1,
    Down = 2,
}

impl Status {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Status::Active,
            1 => Status::Draining,
            _ => Status::Down,
        }
    }
}

/// The swappable part of an instance — replaced atomically on recycle.
struct InstanceInner {
    browser: Arc<Browser>,
    default_user_agent: Arc<String>,
    /// Profile-dir guard. Declared AFTER `browser` so on drop the subprocess
    /// is torn down first, then its profile directory is removed.
    _data_dir: browser::UserDataDir,
}

struct Instance {
    id: usize,
    inner: RwLock<InstanceInner>,
    /// Page-concurrency permits for THIS instance (size = `pages_per_instance`).
    permits: Arc<Semaphore>,
    /// Requests currently being captured on this instance (routing signal).
    in_flight: AtomicUsize,
    /// Requests served since the last (re)spawn (count-based recycle trigger).
    served: AtomicU64,
    status: AtomicU8,
    /// Cached from config so the `Checkout` drop path can decide whether a
    /// completed request crossed the count threshold without the full config.
    recycle_after_requests: u64,
    /// Pinged by `Checkout::drop` when the count threshold is crossed.
    recycle_notify: Notify,
}

impl Instance {
    fn status(&self) -> Status {
        Status::from_u8(self.status.load(Relaxed))
    }

    /// Transition status and keep the active-instances gauge in sync.
    fn set_status(&self, new: Status) {
        let old = Status::from_u8(self.status.swap(new as u8, Relaxed));
        let was_active = old == Status::Active;
        let is_active = new == Status::Active;
        if was_active && !is_active {
            metrics::gauge!(M_ACTIVE_INSTANCES).decrement(1.0);
        } else if !was_active && is_active {
            metrics::gauge!(M_ACTIVE_INSTANCES).increment(1.0);
        }
    }
}

/// A leased page slot on a specific instance. Holds the instance's browser
/// handle for the duration of one capture; releasing it (on drop) frees the
/// permit, updates the in-flight count, and may trigger a count-based recycle.
pub struct Checkout {
    browser: Arc<Browser>,
    default_user_agent: Arc<String>,
    instance: Arc<Instance>,
    _permit: OwnedSemaphorePermit,
}

impl Checkout {
    /// Shared handle to this checkout's browser. Returned as `&Arc<Browser>`
    /// so `browser::capture` can clone it into its detached teardown task.
    pub fn browser(&self) -> &Arc<Browser> {
        &self.browser
    }

    pub fn default_user_agent(&self) -> &str {
        &self.default_user_agent
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        self.instance.in_flight.fetch_sub(1, Relaxed);
        let served = self.instance.served.fetch_add(1, Relaxed) + 1;
        if self.instance.recycle_after_requests > 0
            && served >= self.instance.recycle_after_requests
        {
            // Wake the manager task; it re-checks the threshold before acting.
            self.instance.recycle_notify.notify_one();
        }
    }
}

pub struct BrowserPool {
    instances: Vec<Arc<Instance>>,
    /// Total page permits across the pool (`pool_size × pages_per_instance`) —
    /// the ceiling on simultaneous captures. Used to bound batch fan-out.
    total_permits: usize,
}

impl BrowserPool {
    /// Launch `config.pool_size` chromium instances and spawn a manager task
    /// per instance. Panics if any instance fails its initial launch — the
    /// service can't usefully start without its full pool.
    pub async fn launch(config: PoolConfig) -> Self {
        let recycle_token = Arc::new(Semaphore::new(MAX_UNAVAILABLE));
        let mut instances = Vec::with_capacity(config.pool_size);

        for id in 0..config.pool_size {
            let (browser, ua, disconnect_rx, data_dir) = browser::launch()
                .await
                .unwrap_or_else(|e| panic!("failed to launch browser instance {id}: {e}"));
            let instance = Arc::new(Instance {
                id,
                inner: RwLock::new(InstanceInner {
                    browser: Arc::new(browser),
                    default_user_agent: Arc::new(ua),
                    _data_dir: data_dir,
                }),
                permits: Arc::new(Semaphore::new(config.pages_per_instance)),
                in_flight: AtomicUsize::new(0),
                served: AtomicU64::new(0),
                status: AtomicU8::new(Status::Active as u8),
                recycle_after_requests: config.recycle_after_requests,
                recycle_notify: Notify::new(),
            });
            tokio::spawn(manage_instance(
                instance.clone(),
                recycle_token.clone(),
                config.clone(),
                disconnect_rx,
            ));
            instances.push(instance);
        }

        metrics::gauge!(M_POOL_SIZE).set(config.pool_size as f64);
        metrics::gauge!(M_ACTIVE_INSTANCES).set(config.pool_size as f64);
        tracing::info!(
            pool_size = config.pool_size,
            pages_per_instance = config.pages_per_instance,
            total_concurrency = config.pool_size * config.pages_per_instance,
            recycle_after_requests = config.recycle_after_requests,
            recycle_after_secs = config.recycle_after.map(|d| d.as_secs()).unwrap_or(0),
            "browser pool ready"
        );
        Self {
            instances,
            total_permits: config.pool_size * config.pages_per_instance,
        }
    }

    /// Maximum number of captures that can run at once across the whole pool
    /// (`pool_size × pages_per_instance`).
    pub fn capacity(&self) -> usize {
        self.total_permits
    }

    /// Route a request to the least-loaded active instance, acquiring one of
    /// its page permits (blocking while that instance is saturated). Returns
    /// `Err(())` only when no instance is currently active (all crashed /
    /// recycling); the caller maps that to 503.
    pub async fn checkout(&self) -> Result<Checkout, ()> {
        for _ in 0..CHECKOUT_PICK_RETRIES {
            // Least-loaded = most available permits among active instances.
            let chosen = self
                .instances
                .iter()
                .filter(|i| i.status() == Status::Active)
                .max_by_key(|i| i.permits.available_permits())
                .cloned();
            let Some(inst) = chosen else {
                return Err(());
            };

            // Wait here when the chosen instance is saturated — same queueing
            // behaviour as the original single semaphore.
            let permit = inst
                .permits
                .clone()
                .acquire_owned()
                .await
                .expect("instance semaphore closed");

            // It may have started draining while we waited for a slot; if so,
            // return the permit and re-pick a still-active instance.
            if inst.status() != Status::Active {
                drop(permit);
                continue;
            }

            inst.in_flight.fetch_add(1, Relaxed);
            let (browser, default_user_agent) = {
                let inner = inst.inner.read().await;
                (inner.browser.clone(), inner.default_user_agent.clone())
            };
            return Ok(Checkout {
                browser,
                default_user_agent,
                instance: inst,
                _permit: permit,
            });
        }
        Err(())
    }

    /// A browser handle from the first active instance, for the readiness
    /// probe. `None` when no instance is currently active.
    pub async fn any_active_browser(&self) -> Option<Arc<Browser>> {
        for inst in &self.instances {
            if inst.status() == Status::Active {
                return Some(inst.inner.read().await.browser.clone());
            }
        }
        None
    }
}

#[derive(Clone, Copy)]
enum RecycleReason {
    Crash,
    Age,
    Count,
}

impl RecycleReason {
    fn label(self) -> &'static str {
        match self {
            RecycleReason::Crash => "crash",
            RecycleReason::Age => "age",
            RecycleReason::Count => "count",
        }
    }
}

/// Per-instance supervisor: watches for a crash, age, or count trigger, then
/// drains (for voluntary recycles) and replaces the chromium subprocess.
/// Runs forever as a detached task.
async fn manage_instance(
    instance: Arc<Instance>,
    recycle_token: Arc<Semaphore>,
    config: PoolConfig,
    mut disconnect_rx: oneshot::Receiver<()>,
) {
    let mut spawned_at = Instant::now();
    loop {
        let reason = wait_for_trigger(&instance, &config, &mut disconnect_rx, spawned_at).await;

        match reason {
            RecycleReason::Crash => {
                instance.set_status(Status::Down);
                tracing::error!(
                    instance = instance.id,
                    "browser instance disconnected; recycling"
                );
            }
            RecycleReason::Age | RecycleReason::Count => {
                instance.set_status(Status::Draining);
                tracing::info!(
                    instance = instance.id,
                    reason = reason.label(),
                    "recycling browser instance"
                );
            }
        }

        // Serialize replacements across the pool: at most one instance is ever
        // unavailable at a time, so capacity drops by at most one.
        let _token = recycle_token
            .acquire()
            .await
            .expect("recycle token semaphore closed");

        // Voluntary recycle: let in-flight requests finish first. A crashed
        // browser is already dead, so its in-flight requests are erroring out
        // — skip the drain wait and replace immediately.
        if !matches!(reason, RecycleReason::Crash) {
            drain(&instance, config.drain_timeout).await;
        }

        disconnect_rx = relaunch(&instance).await;
        spawned_at = Instant::now();
        instance.served.store(0, Relaxed);
        instance.set_status(Status::Active);

        match reason {
            RecycleReason::Crash => metrics::counter!(M_RESPAWNS).increment(1),
            RecycleReason::Age | RecycleReason::Count => {
                metrics::counter!(M_RECYCLES, "reason" => reason.label()).increment(1)
            }
        }
        tracing::info!(
            instance = instance.id,
            reason = reason.label(),
            "browser instance back in service"
        );
    }
}

/// Block until this instance should be recycled, returning why. Spurious
/// count notifications (already handled) are ignored and waiting resumes.
async fn wait_for_trigger(
    instance: &Arc<Instance>,
    config: &PoolConfig,
    disconnect_rx: &mut oneshot::Receiver<()>,
    spawned_at: Instant,
) -> RecycleReason {
    loop {
        let age_sleep = async {
            match config.recycle_after {
                Some(d) => {
                    let deadline = spawned_at + d;
                    tokio::time::sleep(deadline.saturating_duration_since(Instant::now())).await;
                }
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            _ = &mut *disconnect_rx => return RecycleReason::Crash,
            _ = age_sleep => return RecycleReason::Age,
            _ = instance.recycle_notify.notified() => {
                if config.recycle_after_requests > 0
                    && instance.served.load(Relaxed) >= config.recycle_after_requests
                {
                    return RecycleReason::Count;
                }
                // else: stale wake-up (already recycled) — keep waiting.
            }
        }
    }
}

/// Wait for the instance's in-flight requests to reach zero, up to
/// `timeout`. On timeout we recycle anyway — dropping the old browser cancels
/// any stuck capture.
async fn drain(instance: &Arc<Instance>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while instance.in_flight.load(Relaxed) > 0 {
        if Instant::now() >= deadline {
            tracing::warn!(
                instance = instance.id,
                in_flight = instance.in_flight.load(Relaxed),
                "drain timeout; recycling with requests still in flight"
            );
            return;
        }
        tokio::time::sleep(DRAIN_POLL).await;
    }
}

/// Launch a fresh chromium and swap it into the instance, retrying with
/// exponential backoff. Returns the new browser's disconnect receiver.
async fn relaunch(instance: &Arc<Instance>) -> oneshot::Receiver<()> {
    let mut backoff = Duration::from_secs(1);
    loop {
        match browser::launch().await {
            Ok((browser, ua, disconnect_rx, data_dir)) => {
                let (old_browser, old_dir) = {
                    let mut inner = instance.inner.write().await;
                    inner.default_user_agent = Arc::new(ua);
                    let old_browser = std::mem::replace(&mut inner.browser, Arc::new(browser));
                    let old_dir = std::mem::replace(&mut inner._data_dir, data_dir);
                    (old_browser, old_dir)
                };
                // Last `Arc<Browser>` ref → chromiumoxide's Drop tears down the
                // old subprocess (after drain there are no other holders); then
                // remove its now-released profile directory.
                drop(old_browser);
                drop(old_dir);
                return disconnect_rx;
            }
            Err(e) => {
                tracing::error!(
                    instance = instance.id,
                    error = %e,
                    retry_in = ?backoff,
                    "instance relaunch failed"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}
