//! The worker: one thread with its own small tokio runtime, so an app with
//! no async runtime (or another one) can emit. It takes events off a
//! channel, writes them to the outbox, and delivers what is due, every due
//! delivery at once.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::http::{self, Answer, Endpoint};
use crate::outbox::{Entry, Outbox};
use crate::sign;
use crate::{Config, Status, Target, TargetStatus};

pub(crate) enum Msg {
    Emit { event: String, body: String, created: u64 },
    Targets(Vec<Target>),
}

/// What the handle and the worker share.
#[derive(Default)]
pub(crate) struct Shared {
    pub status: Mutex<Status>,
    /// Messages sent and not yet taken in by the worker.
    pub queued: AtomicUsize,
}

/// How one attempt ended.
enum Outcome {
    /// With what the receiver said back: what a caller waiting on a reply
    /// (a session id in `bug.filed`'s, for a follow-up long poll) would be
    /// handed. Nothing reads it yet.
    #[allow(dead_code)]
    Delivered(Answer),
    /// Refused for good: retrying won't change the answer.
    Refused(u16),
    /// Try again, not before the receiver's `Retry-After` (seconds).
    Failed(String, Option<u64>),
}

fn classify(r: Result<Answer, String>) -> Outcome {
    match r {
        Ok(a) if (200..=299).contains(&a.code) => Outcome::Delivered(a),
        Ok(a) if matches!(a.code, 408 | 425 | 429 | 500..=599) => {
            Outcome::Failed(format!("HTTP {}", a.code), a.retry_after())
        }
        Ok(a) => Outcome::Refused(a.code),
        Err(e) => Outcome::Failed(e, None),
    }
}

/// The longest `Retry-After` honoured: a receiver can't park a delivery
/// for longer than this.
const MAX_RETRY_AFTER: u64 = 3600;

pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

struct Worker {
    config: Config,
    targets: Vec<(Target, Option<Endpoint>)>,
    outbox: Outbox,
    shared: Arc<Shared>,
    /// Per URL: delivered, refused or expired counts and the last outcome.
    seen: BTreeMap<String, TargetStatus>,
}

pub(crate) fn spawn(
    config: Config,
    shared: Arc<Shared>,
    rx: mpsc::UnboundedReceiver<Msg>,
) -> std::io::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    std::thread::Builder::new().name("simply-hooks".into()).spawn(move || {
        let outbox = Outbox::open(config.outbox.clone());
        let mut w = Worker { config, targets: vec![], outbox, shared, seen: BTreeMap::new() };
        let targets = std::mem::take(&mut w.config.targets);
        w.set_targets(targets);
        rt.block_on(w.run(rx));
    })?;
    Ok(())
}

impl Worker {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        loop {
            let wait = match self.outbox.next_due() {
                Some(due) => Duration::from_millis(due.saturating_sub(now_ms())),
                None => Duration::from_secs(3600),
            };
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    self.take(msg);
                    // Everything already sent, in one go: one write for a burst.
                    while let Ok(msg) = rx.try_recv() {
                        self.take(msg);
                    }
                    self.outbox.save();
                }
                _ = tokio::time::sleep(wait) => {}
            }
            self.deliver_due().await;
            self.publish();
        }
    }

    fn take(&mut self, msg: Msg) {
        match msg {
            Msg::Emit { event, body, created } => {
                for (t, _) in self.targets.iter().filter(|(t, e)| e.is_some() && t.allows(&event)) {
                    self.outbox.entries.push(Entry {
                        id: uuid::Uuid::new_v4().to_string(),
                        event: event.clone(),
                        url: t.url.clone(),
                        body: body.clone(),
                        created,
                        attempts: 0,
                        next_ms: 0,
                    });
                }
                let over = self.outbox.entries.len().saturating_sub(self.config.max_pending);
                if over > 0 {
                    tracing::warn!("webhooks: outbox full, dropping the {over} oldest deliveries");
                    self.outbox.entries.drain(..over);
                }
            }
            Msg::Targets(targets) => self.set_targets(targets),
        }
        self.shared.queued.fetch_sub(1, Ordering::SeqCst);
    }

    /// Take `targets`; deliveries to a URL no longer among them are dropped.
    fn set_targets(&mut self, targets: Vec<Target>) {
        self.targets = targets
            .into_iter()
            .map(|t| {
                let e = Endpoint::parse(&t.url)
                    .inspect_err(|e| tracing::warn!("webhooks: target not used: {e}"))
                    .ok();
                (t, e)
            })
            .collect();
        let before = self.outbox.entries.len();
        let targets = &self.targets;
        self.outbox
            .entries
            .retain(|e| targets.iter().any(|(t, ok)| ok.is_some() && t.url == e.url));
        if self.outbox.entries.len() != before {
            self.outbox.save();
        }
        self.seen.retain(|url, _| targets.iter().any(|(t, _)| &t.url == url));
        self.publish();
    }

    async fn deliver_due(&mut self) {
        let now = now_ms();
        let max_age = self.config.retry.max_age.as_secs();
        let mut changed = false;
        // Too old: dropped before another try.
        let (old, keep): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut self.outbox.entries)
            .into_iter()
            .partition(|e| (now / 1000).saturating_sub(e.created) > max_age);
        self.outbox.entries = keep;
        for e in old {
            changed = true;
            tracing::warn!(
                "webhooks: gave up on {} delivery {} after {} attempts: older than {} s",
                e.event,
                e.id,
                e.attempts,
                max_age
            );
            self.status_of(&e.url).dropped += 1;
        }
        let mut set = JoinSet::new();
        for (i, e) in self.outbox.entries.iter().enumerate().filter(|(_, e)| e.next_ms <= now) {
            let Some((target, Some(endpoint))) = self.targets.iter().find(|(t, _)| t.url == e.url)
            else {
                continue;
            };
            let ts = now / 1000;
            let mut headers = vec![
                (sign::EVENT_HEADER, e.event.clone()),
                (sign::DELIVERY_HEADER, e.id.clone()),
                ("User-Agent", self.config.user_agent.clone()),
            ];
            if let Some(secret) = &target.secret {
                headers.push((sign::TIMESTAMP_HEADER, ts.to_string()));
                headers.push((
                    sign::SIGNATURE_HEADER,
                    sign::sign(secret.0.as_bytes(), ts, e.body.as_bytes()),
                ));
            }
            let (endpoint, body, timeout) = (endpoint.clone(), e.body.clone(), self.config.timeout);
            set.spawn(async move {
                (i, http::post(&endpoint, &headers, body.as_bytes(), timeout).await)
            });
        }
        let mut done = vec![];
        while let Some(joined) = set.join_next().await {
            let Ok((i, r)) = joined else { continue };
            changed = true;
            let at = SystemTime::now();
            let e = &mut self.outbox.entries[i];
            e.attempts += 1;
            let host = Endpoint::parse(&e.url).map(|p| p.host().to_owned()).unwrap_or_default();
            let (event, id, url, attempts) =
                (e.event.clone(), e.id.clone(), e.url.clone(), e.attempts);
            match classify(r) {
                Outcome::Delivered(_) => {
                    done.push(i);
                    let s = self.status_of(&url);
                    s.delivered += 1;
                    s.last_ok = Some(at);
                    s.last_error = None;
                }
                Outcome::Refused(code) => {
                    done.push(i);
                    tracing::warn!(
                        "webhooks: {host} refused {event} delivery {id} (HTTP {code}): dropped"
                    );
                    let s = self.status_of(&url);
                    s.dropped += 1;
                    s.last_error = Some(format!("refused: HTTP {code}"));
                    s.last_error_at = Some(at);
                }
                Outcome::Failed(why, retry_after) => {
                    let asked = Duration::from_secs(retry_after.unwrap_or(0).min(MAX_RETRY_AFTER));
                    let delay = self.config.retry.after(attempts).max(asked);
                    self.outbox.entries[i].next_ms = now_ms() + delay.as_millis() as u64;
                    tracing::warn!(
                        "webhooks: {event} delivery {id} to {host} failed (attempt {attempts}): {why}; again in {} s",
                        delay.as_secs()
                    );
                    let s = self.status_of(&url);
                    s.last_error = Some(why);
                    s.last_error_at = Some(at);
                }
            }
        }
        done.sort_unstable();
        for i in done.into_iter().rev() {
            self.outbox.entries.remove(i);
        }
        if changed {
            self.outbox.save();
        }
    }

    fn status_of(&mut self, url: &str) -> &mut TargetStatus {
        self.seen
            .entry(url.to_owned())
            .or_insert_with(|| TargetStatus { url: url.to_owned(), ..Default::default() })
    }

    fn publish(&mut self) {
        let mut targets = vec![];
        for (t, endpoint) in &self.targets {
            let mut s = self.seen.get(&t.url).cloned().unwrap_or_default();
            s.url = t.url.clone();
            s.pending = self.outbox.entries.iter().filter(|e| e.url == t.url).count();
            if endpoint.is_none() {
                s.last_error = Endpoint::parse(&t.url).err();
            }
            targets.push(s);
        }
        let pending = self.outbox.entries.len();
        *self.shared.status.lock().unwrap_or_else(|p| p.into_inner()) = Status { pending, targets };
    }
}
