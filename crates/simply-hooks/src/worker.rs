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
use crate::{Config, Delivery, DeliveryState, DropReason, Status, Target, TargetStatus};

pub(crate) enum Msg {
    Emit { id: String, event: String, body: String, created: u64, seq: Option<String> },
    Targets(Vec<Target>),
}

/// What the handle and the worker share.
#[derive(Default)]
pub(crate) struct Shared {
    pub status: Mutex<Status>,
    /// Messages sent and not yet taken in by the worker.
    pub queued: AtomicUsize,
    /// Each event's deliveries ([`crate::Hooks::deliveries`]).
    pub events: Mutex<Events>,
}

/// One event's deliveries, and when it ended (`None`: one is pending, or
/// it is not routed yet), in the order events end.
#[derive(Default)]
pub(crate) struct Event {
    pub deliveries: Vec<Delivery>,
    ended: Option<u64>,
}

/// Every event emitted and not forgotten, by id.
#[derive(Default)]
pub(crate) struct Events {
    map: BTreeMap<String, Event>,
    /// How many events have ended: the next one's place.
    ends: u64,
}

impl Events {
    pub fn get(&self, id: &str) -> Option<&Event> {
        self.map.get(id)
    }

    /// `emit` gave out `id`: known, not routed yet.
    pub fn emitted(&mut self, id: &str) {
        self.map.insert(id.to_owned(), Event::default());
    }

    pub fn forget(&mut self, id: &str) {
        self.map.remove(id);
    }

    /// `id` went to these targets (possibly none).
    fn routed(&mut self, id: &str, deliveries: Vec<Delivery>) {
        self.map.entry(id.to_owned()).or_default().deliveries.extend(deliveries);
        self.end_if_done(id);
    }

    /// A delivery of `event` now stands at `state`.
    fn set(&mut self, event: Option<&str>, delivery: &str, state: DeliveryState) {
        let Some(event) = event else { return };
        let Some(e) = self.map.get_mut(event) else { return };
        if let Some(d) = e.deliveries.iter_mut().find(|d| d.id == delivery) {
            d.state = state;
        }
        self.end_if_done(event);
    }

    fn end_if_done(&mut self, id: &str) {
        let Some(e) = self.map.get_mut(id) else { return };
        if e.ended.is_none() && e.deliveries.iter().all(|d| !d.state.pending()) {
            e.ended = Some(self.ends);
            self.ends += 1;
        }
    }

    /// Keep the newest `keep` events that ended, and every one that hasn't.
    fn prune(&mut self, keep: usize) {
        let ended = self.map.values().filter(|e| e.ended.is_some()).count();
        if ended <= keep {
            return;
        }
        let mut ends: Vec<u64> = self.map.values().filter_map(|e| e.ended).collect();
        ends.sort_unstable();
        let cut = ends[ended - keep];
        self.map.retain(|_, e| e.ended.is_none_or(|n| n >= cut));
    }
}

fn at_ms(ms: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms)
}

/// A pending delivery's state, as its outbox entry says.
fn pending_state(e: &Entry, ready: bool) -> DeliveryState {
    DeliveryState::Pending {
        attempts: e.attempts,
        last_error: e.last_error.clone(),
        next_try: ready.then(|| at_ms(e.next_ms)),
    }
}

fn dropped(e: &Entry, reason: DropReason) -> DeliveryState {
    DeliveryState::Dropped { at: SystemTime::now(), attempts: e.attempts, reason }
}

/// How one attempt ended.
enum Outcome {
    /// With what the receiver said back: what a caller waiting on a reply
    /// (a session id in `bug.filed`'s, for a follow-up long poll) would be
    /// handed. Nothing reads it yet.
    #[allow(dead_code)]
    Delivered(Answer),
    /// Refused for good: retrying won't change the answer. With the
    /// receiver's reason, when its answer is `{"error": "…"}`.
    Refused(u16, Option<String>),
    /// Try again, not before the receiver's `Retry-After` (seconds).
    Failed(String, Option<u64>),
}

fn classify(r: Result<Answer, String>) -> Outcome {
    match r {
        Ok(a) if (200..=299).contains(&a.code) => Outcome::Delivered(a),
        Ok(a) if matches!(a.code, 408 | 425 | 429 | 500..=599) => {
            Outcome::Failed(format!("HTTP {}", a.code), a.retry_after())
        }
        Ok(a) => {
            let why = serde_json::from_slice::<serde_json::Value>(&a.body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
                .map(|e| e.chars().take(120).collect());
            Outcome::Refused(a.code, why)
        }
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
    /// Sequences given up on, per URL and name, with when (unix ms) and
    /// why: what is emitted in one later is dropped. Forgotten after the
    /// max age.
    closed: BTreeMap<(String, String), (u64, DropReason)>,
}

pub(crate) fn spawn(
    config: Config,
    shared: Arc<Shared>,
    rx: mpsc::UnboundedReceiver<Msg>,
) -> std::io::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    std::thread::Builder::new().name("simply-hooks".into()).spawn(move || {
        let outbox = Outbox::open(config.outbox.clone());
        let mut w = Worker {
            config,
            targets: vec![],
            outbox,
            shared,
            seen: BTreeMap::new(),
            closed: BTreeMap::new(),
        };
        // What is pending is known by its event across a restart.
        {
            let mut events = w.shared.events.lock().unwrap_or_else(|p| p.into_inner());
            let ready = w.outbox.ready();
            for (e, r) in w.outbox.entries.iter().zip(ready) {
                if let Some(id) = &e.event_id {
                    let d = Delivery {
                        url: e.url.clone(),
                        id: e.id.clone(),
                        state: pending_state(e, r),
                    };
                    events.routed(id, vec![d]);
                }
            }
        }
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
                    let mut taken = 1;
                    // Everything already sent, in one go: one write for a burst.
                    while let Ok(msg) = rx.try_recv() {
                        self.take(msg);
                        taken += 1;
                    }
                    self.outbox.save();
                    // Taken in only once the status shows it: `wait_idle`
                    // never sees an emit neither queued nor pending.
                    self.publish();
                    self.shared.queued.fetch_sub(taken, Ordering::SeqCst);
                }
                _ = tokio::time::sleep(wait) => {}
            }
            self.deliver_due().await;
            self.publish();
        }
    }

    fn take(&mut self, msg: Msg) {
        match msg {
            Msg::Emit { id: event_id, event, body, created, seq } => {
                let mut routed = vec![];
                for (t, _) in self.targets.iter().filter(|(t, e)| e.is_some() && t.allows(&event)) {
                    let id = uuid::Uuid::new_v4().to_string();
                    // A sequence given up on takes nothing more: never queued, as
                    // a body over the limit isn't, so not counted as dropped.
                    let closed =
                        seq.as_ref().and_then(|s| self.closed.get(&(t.url.clone(), s.clone())));
                    if let Some((_, cause)) = closed {
                        tracing::warn!("webhooks: {event} not sent: an earlier delivery in its sequence was given up on");
                        let reason = DropReason::Sequence { cause: Box::new(cause.clone()) };
                        let state =
                            DeliveryState::Dropped { at: SystemTime::now(), attempts: 0, reason };
                        routed.push(Delivery { url: t.url.clone(), id, state });
                        continue;
                    }
                    let entry = Entry {
                        id,
                        event: event.clone(),
                        url: t.url.clone(),
                        body: body.clone(),
                        created,
                        attempts: 0,
                        next_ms: 0,
                        seq: seq.clone(),
                        event_id: Some(event_id.clone()),
                        last_error: None,
                    };
                    let state = pending_state(&entry, false);
                    routed.push(Delivery { url: entry.url.clone(), id: entry.id.clone(), state });
                    self.outbox.entries.push(entry);
                }
                self.events().routed(&event_id, routed);
                let over = self.outbox.entries.len().saturating_sub(self.config.max_pending);
                if over > 0 {
                    tracing::warn!("webhooks: outbox full, dropping the {over} oldest deliveries");
                    let dropped: Vec<Entry> = self.outbox.entries.drain(..over).collect();
                    let now = now_ms();
                    for e in &dropped {
                        self.give_up(e, now, DropReason::Evicted);
                    }
                    self.drop_closed();
                }
            }
            Msg::Targets(targets) => self.set_targets(targets),
        }
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
        let targets = &self.targets;
        let (keep, gone): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut self.outbox.entries)
            .into_iter()
            .partition(|e| targets.iter().any(|(t, ok)| ok.is_some() && t.url == e.url));
        self.outbox.entries = keep;
        if !gone.is_empty() {
            let mut events = self.events();
            for e in &gone {
                events.set(e.event_id.as_deref(), &e.id, dropped(e, DropReason::TargetRemoved));
            }
            drop(events);
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
            self.give_up(&e, now, DropReason::Expired);
            self.events().set(e.event_id.as_deref(), &e.id, dropped(&e, DropReason::Expired));
            tracing::warn!(
                "webhooks: gave up on {} delivery {} after {} attempts: older than {} s",
                e.event,
                e.id,
                e.attempts,
                max_age
            );
            self.status_of(&e.url).dropped += 1;
        }
        // Before anything is tried: what waited on an entry given up on goes too.
        changed |= self.drop_closed();
        let mut set = JoinSet::new();
        let ready = self.outbox.ready();
        for (i, e) in
            self.outbox.entries.iter().enumerate().filter(|(i, e)| ready[*i] && e.next_ms <= now)
        {
            let Some((target, Some(endpoint))) = self.targets.iter().find(|(t, _)| t.url == e.url)
            else {
                continue;
            };
            let ts = now / 1000;
            let mut headers = vec![
                (sign::EVENT_HEADER.to_owned(), e.event.clone()),
                (sign::DELIVERY_HEADER.to_owned(), e.id.clone()),
                ("User-Agent".to_owned(), self.config.user_agent.clone()),
            ];
            if let Some(secret) = &target.secret {
                headers.push((sign::TIMESTAMP_HEADER.to_owned(), ts.to_string()));
                headers.push((
                    sign::SIGNATURE_HEADER.to_owned(),
                    sign::sign(secret.0.as_bytes(), ts, e.body.as_bytes()),
                ));
            }
            headers.extend(target.headers.iter().cloned());
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
            let event_id = e.event_id.clone();
            match classify(r) {
                Outcome::Delivered(_) => {
                    done.push(i);
                    let state = DeliveryState::Delivered { at, attempts };
                    self.events().set(event_id.as_deref(), &id, state);
                    let s = self.status_of(&url);
                    s.delivered += 1;
                    s.last_ok = Some(at);
                    s.last_error = None;
                }
                Outcome::Refused(code, why) => {
                    done.push(i);
                    let e = self.outbox.entries[i].clone();
                    let reason = DropReason::Refused { status: code, error: why.clone() };
                    self.give_up(&e, now_ms(), reason.clone());
                    self.events().set(event_id.as_deref(), &id, dropped(&e, reason));
                    let why = why.map(|w| format!(" ({w})")).unwrap_or_default();
                    tracing::warn!(
                        "webhooks: {host} refused {event} delivery {id} (HTTP {code}{why}): dropped"
                    );
                    let s = self.status_of(&url);
                    s.dropped += 1;
                    s.last_error = Some(format!("refused: HTTP {code}{why}"));
                    s.last_error_at = Some(at);
                }
                Outcome::Failed(why, retry_after) => {
                    let asked = Duration::from_secs(retry_after.unwrap_or(0).min(MAX_RETRY_AFTER));
                    let delay = self.config.retry.after(attempts).max(asked);
                    self.outbox.entries[i].next_ms = now_ms() + delay.as_millis() as u64;
                    self.outbox.entries[i].last_error = Some(why.clone());
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
        changed |= self.drop_closed();
        let max_age_ms = self.config.retry.max_age.as_millis() as u64;
        self.closed.retain(|_, (at, _)| now.saturating_sub(*at) <= max_age_ms);
        if changed {
            self.outbox.save();
        }
    }

    /// `e` was given up on, for `why`: so is its sequence, if it is in one
    /// (for the first such reason).
    fn give_up(&mut self, e: &Entry, now: u64, why: DropReason) {
        if let Some((url, seq)) = e.chain() {
            self.closed.entry((url.to_owned(), seq.to_owned())).or_insert((now, why));
        }
    }

    fn events(&self) -> std::sync::MutexGuard<'_, Events> {
        self.shared.events.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Drop what is left of the sequences given up on, each counted as
    /// dropped: whether any was.
    fn drop_closed(&mut self) -> bool {
        let (gone, keep): (Vec<Entry>, Vec<Entry>) =
            std::mem::take(&mut self.outbox.entries).into_iter().partition(|e| {
                e.chain()
                    .is_some_and(|(u, s)| self.closed.contains_key(&(u.to_owned(), s.to_owned())))
            });
        self.outbox.entries = keep;
        for e in &gone {
            tracing::warn!(
                "webhooks: {} delivery {} dropped: an earlier delivery in its sequence was given up on",
                e.event,
                e.id
            );
            let cause = e
                .chain()
                .and_then(|(u, s)| self.closed.get(&(u.to_owned(), s.to_owned())))
                .map_or(DropReason::Expired, |(_, why)| why.clone());
            let reason = DropReason::Sequence { cause: Box::new(cause) };
            self.events().set(e.event_id.as_deref(), &e.id, dropped(e, reason));
            self.status_of(&e.url).dropped += 1;
        }
        !gone.is_empty()
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
        let mut events = self.events();
        for (e, r) in self.outbox.entries.iter().zip(self.outbox.ready()) {
            events.set(e.event_id.as_deref(), &e.id, pending_state(e, r));
        }
        events.prune(self.config.max_pending);
    }
}
