//! Deliveries against a receiver on loopback: what is sent and how it is
//! signed, retries, the outbox across a restart, the allowlist, and an
//! `emit` that returns while a delivery hangs.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use simply_hooks::{sign, Config, DeliveryState, DropReason, EventId, Hooks, Retry, Target};

/// One request as received.
#[derive(Clone, Debug)]
struct Got {
    path: String,
    headers: BTreeMap<String, String>,
    body: String,
}

impl Got {
    fn header(&self, name: &str) -> &str {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str).unwrap_or("")
    }
}

/// How the receiver answers the next request.
#[derive(Clone, Copy)]
enum Answer {
    Status(u16),
    /// A status with `Retry-After: <seconds>`.
    RetryAfter(u16, u64),
    /// Read the request, never answer.
    Hang,
    /// A status with `{"error": <reason>}`.
    Refuse(u16, &'static str),
}

/// A receiver: answers from a script (then 200), records every request.
/// One that is down closes each connection at once, unread and unrecorded:
/// every attempt fails, as with nobody there, while the port stays this
/// test's (a port let go to be bound again later can be taken meanwhile).
#[derive(Clone)]
struct Receiver {
    url: String,
    got: Arc<Mutex<Vec<Got>>>,
    script: Arc<Mutex<VecDeque<Answer>>>,
    down: Arc<AtomicBool>,
}

impl Receiver {
    fn start(script: impl IntoIterator<Item = Answer>) -> Receiver {
        Receiver::new(false, script)
    }

    /// A receiver that is down until [`Receiver::up`].
    fn down() -> Receiver {
        Receiver::new(true, [])
    }

    fn up(&self) {
        self.down.store(false, Ordering::SeqCst);
    }

    fn new(down: bool, script: impl IntoIterator<Item = Answer>) -> Receiver {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/hooks/in", listener.local_addr().unwrap().port());
        let r = Receiver {
            url,
            got: Arc::default(),
            script: Arc::new(Mutex::new(script.into_iter().collect())),
            down: Arc::new(AtomicBool::new(down)),
        };
        let me = r.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                if me.down.load(Ordering::SeqCst) {
                    drop(stream);
                    continue;
                }
                let me = me.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let path = line.split(' ').nth(1).unwrap_or("").to_owned();
                    let mut headers = BTreeMap::new();
                    loop {
                        let mut h = String::new();
                        reader.read_line(&mut h).unwrap();
                        let h = h.trim_end();
                        if h.is_empty() {
                            break;
                        }
                        let (k, v) = h.split_once(':').unwrap();
                        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
                    }
                    let n: usize = headers.get("content-length").unwrap().parse().unwrap();
                    let mut body = vec![0; n];
                    reader.read_exact(&mut body).unwrap();
                    me.got.lock().unwrap().push(Got {
                        path,
                        headers,
                        body: String::from_utf8(body).unwrap(),
                    });
                    let answer =
                        me.script.lock().unwrap().pop_front().unwrap_or(Answer::Status(200));
                    match answer {
                        Answer::Status(code) => {
                            let _ =
                                write!(stream, "HTTP/1.1 {code} X\r\nContent-Length: 2\r\n\r\nok");
                        }
                        Answer::RetryAfter(code, secs) => {
                            let _ = write!(stream, "HTTP/1.1 {code} X\r\nRetry-After: {secs}\r\nContent-Length: 0\r\n\r\n");
                        }
                        Answer::Hang => std::thread::sleep(Duration::from_secs(30)),
                        Answer::Refuse(code, why) => {
                            let body = format!("{{\"error\":\"{why}\"}}");
                            let _ = write!(
                                stream,
                                "HTTP/1.1 {code} X\r\nContent-Length: {}\r\n\r\n{body}",
                                body.len()
                            );
                        }
                    }
                });
            }
        });
        r
    }

    fn got(&self) -> Vec<Got> {
        self.got.lock().unwrap().clone()
    }

    fn wait_for(&self, n: usize) -> Vec<Got> {
        let until = Instant::now() + Duration::from_secs(10);
        while self.got().len() < n {
            assert!(Instant::now() < until, "waited for {n} requests, got {}", self.got().len());
            std::thread::sleep(Duration::from_millis(10));
        }
        self.got()
    }
}

fn fast() -> Retry {
    Retry {
        first: Duration::from_millis(100),
        max: Duration::from_millis(400),
        max_age: Duration::from_secs(3600),
    }
}

fn config(outbox: Option<PathBuf>) -> Config {
    Config::new(outbox, "simply-hooks-test/1").retry(fast()).timeout(Duration::from_secs(2))
}

fn check_signed(g: &Got, secret: &str) {
    let ts: u64 = g.header("x-timestamp").parse().unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(now.abs_diff(ts) <= 5, "the timestamp is this attempt's");
    let sig = g.header("x-signature");
    assert!(sig.starts_with("sha256=") && sig.len() == 7 + 64, "{sig}");
    assert_eq!(sig, sign::sign(secret.as_bytes(), ts, g.body.as_bytes()));
    assert!(sign::verify(secret.as_bytes(), ts, g.body.as_bytes(), sig));
}

#[test]
fn an_event_arrives_signed_in_the_envelope() {
    let rx = Receiver::start([]);
    let hooks = Hooks::new(config(None).target(Target::new(&rx.url, "s3cret"))).unwrap();
    hooks.emit("app.started", serde_json::json!({ "version": "1.2.3", "os": "macos" }));
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    let got = rx.wait_for(1);
    let g = &got[0];
    assert_eq!(g.path, "/hooks/in");
    assert_eq!(g.header("content-type"), "application/json");
    assert_eq!(g.header("x-hook-event"), "app.started");
    assert_eq!(g.header("user-agent"), "simply-hooks-test/1");
    let id = g.header("x-hook-delivery");
    assert_eq!((id.len(), id.as_bytes()[14]), (36, b'4'), "a UUID v4: {id}");
    check_signed(g, "s3cret");
    let body: serde_json::Value = serde_json::from_str(&g.body).unwrap();
    assert_eq!(body["event"], "app.started");
    assert_eq!(body["data"], serde_json::json!({ "version": "1.2.3", "os": "macos" }));
    let at = body["at"].as_str().unwrap();
    assert!(at.ends_with('Z') && at.len() == 20, "RFC 3339 UTC to the second: {at}");
    assert_eq!(body.as_object().unwrap().len(), 3);
    let s = hooks.status();
    assert_eq!((s.pending, s.targets[0].delivered, s.targets[0].last_error.clone()), (0, 1, None));
    assert!(s.targets[0].last_ok.is_some());
}

#[test]
fn a_retry_keeps_its_delivery_id_and_signs_again() {
    let rx = Receiver::start([Answer::Status(503), Answer::Status(429)]);
    let hooks = Hooks::new(config(None).target(Target::new(&rx.url, "k"))).unwrap();
    hooks.emit("jam.started", serde_json::json!({ "session": "a" }));
    let got = rx.wait_for(3);
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    let ids: Vec<&str> = got.iter().map(|g| g.header("x-hook-delivery")).collect();
    assert!(ids.iter().all(|i| *i == ids[0]), "{ids:?}");
    assert!(got.iter().all(|g| g.body == got[0].body), "the same body");
    for g in &got {
        check_signed(g, "k");
    }
    let s = &hooks.status().targets[0];
    assert_eq!((s.delivered, s.dropped, s.pending), (1, 0, 0));
}

#[test]
fn a_refusal_is_not_retried() {
    let rx = Receiver::start([Answer::Status(401)]);
    let hooks = Hooks::new(config(None).target(Target::new(&rx.url, "wrong"))).unwrap();
    hooks.emit("jam.ended", serde_json::json!({}));
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(rx.got().len(), 1, "one attempt");
    let s = &hooks.status().targets[0];
    assert_eq!((s.delivered, s.dropped), (0, 1));
    assert_eq!(s.last_error.as_deref(), Some("refused: HTTP 401"));
}

#[test]
fn the_outbox_outlives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = dir.path().join("hooks-outbox.json");
    // A receiver that is down: every attempt fails.
    let rx = Receiver::down();
    let url = rx.url.clone();
    {
        let hooks =
            Hooks::new(config(Some(outbox.clone())).target(Target::new(&url, "k"))).unwrap();
        hooks.emit("app.started", serde_json::json!({ "n": 1 }));
        let until = Instant::now() + Duration::from_secs(10);
        while hooks.status().targets.first().is_none_or(|t| t.last_error.is_none()) {
            assert!(Instant::now() < until, "an attempt failed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(hooks.status().pending, 1);
    }
    std::thread::sleep(Duration::from_millis(200));
    let kept = std::fs::read_to_string(&outbox).unwrap();
    assert!(
        kept.contains("app.started") && !kept.contains("\"k\""),
        "kept, without the secret: {kept}"
    );
    let id = serde_json::from_str::<serde_json::Value>(&kept).unwrap()["entries"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // The app starts again, the receiver is up.
    rx.up();
    let hooks = Hooks::new(config(Some(outbox.clone())).target(Target::new(&url, "k"))).unwrap();
    let got = rx.wait_for(1);
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    assert_eq!(got[0].header("x-hook-delivery"), id, "the same delivery");
    check_signed(&got[0], "k");
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&outbox).unwrap()).unwrap();
    assert_eq!(after["entries"], serde_json::json!([]), "pruned once delivered");
}

#[test]
fn a_target_gone_from_the_settings_takes_its_deliveries_with_it() {
    let rx = Receiver::start([Answer::Status(500)]);
    let hooks = Hooks::new(
        config(None)
            .retry(Retry { first: Duration::from_secs(60), ..fast() })
            .target(Target::new(&rx.url, "k")),
    )
    .unwrap();
    hooks.emit("app.started", serde_json::json!({}));
    rx.wait_for(1);
    let until = Instant::now() + Duration::from_secs(10);
    while hooks.status().pending != 1 || hooks.status().targets[0].last_error.is_none() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    hooks.set_targets(vec![]);
    assert!(hooks.wait_idle(Duration::from_secs(5)));
    assert_eq!(hooks.status().targets.len(), 0);
}

#[test]
fn each_target_gets_only_the_events_it_allows() {
    let (jams, all) = (Receiver::start([]), Receiver::start([]));
    let hooks = Hooks::new(
        config(None)
            .target(Target::new(&jams.url, "a").events(["jam.*"]))
            .target(Target::new(&all.url, "b")),
    )
    .unwrap();
    hooks.emit("app.started", serde_json::json!({}));
    hooks.emit("jam.started", serde_json::json!({}));
    hooks.emit("jam.ended", serde_json::json!({}));
    hooks.emit("Not An Event", serde_json::json!({}));
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    let names = |r: &Receiver| {
        let mut v: Vec<String> =
            r.got().iter().map(|g| g.header("x-hook-event").to_owned()).collect();
        v.sort();
        v
    };
    assert_eq!(names(&jams), ["jam.ended", "jam.started"]);
    assert_eq!(names(&all), ["app.started", "jam.ended", "jam.started"]);
    for g in jams.got() {
        check_signed(&g, "a");
    }
    for g in all.got() {
        check_signed(&g, "b");
    }
}

#[test]
fn emit_returns_while_a_delivery_hangs() {
    let rx = Receiver::start([Answer::Hang]);
    let hooks =
        Hooks::new(config(None).timeout(Duration::from_secs(5)).target(Target::new(&rx.url, "k")))
            .unwrap();
    hooks.emit("app.started", serde_json::json!({}));
    rx.wait_for(1); // the worker is now waiting on an answer that won't come
    let t = Instant::now();
    for i in 0..200 {
        hooks.emit("jam.started", serde_json::json!({ "i": i, "pad": "x".repeat(1000) }));
    }
    let took = t.elapsed();
    assert!(took < Duration::from_millis(200), "200 emits took {took:?}");
}

#[test]
fn a_bad_url_is_said_and_never_dialled() {
    let hooks = Hooks::new(config(None).target(Target::new("http://example.com/x", "k"))).unwrap();
    hooks.emit("app.started", serde_json::json!({}));
    assert!(hooks.wait_idle(Duration::from_secs(5)));
    let s = hooks.status();
    assert_eq!(s.pending, 0);
    assert!(s.targets[0].last_error.as_deref().unwrap().contains("https://"), "{s:?}");
}

#[test]
fn retry_after_is_honoured_when_later_than_the_backoff() {
    let rx = Receiver::start([Answer::RetryAfter(429, 1)]);
    let hooks = Hooks::new(config(None).target(Target::new(&rx.url, "k"))).unwrap();
    let t = Instant::now();
    hooks.emit("bug.filed", serde_json::json!({}));
    let got = rx.wait_for(2);
    assert!(
        t.elapsed() >= Duration::from_millis(900),
        "waited as asked, not 100 ms: {:?}",
        t.elapsed()
    );
    assert_eq!(got[0].header("x-hook-delivery"), got[1].header("x-hook-delivery"));
}

#[test]
fn a_body_over_the_limit_is_never_queued() {
    let rx = Receiver::start([]);
    let hooks = Hooks::new(config(None).max_body(1024).target(Target::new(&rx.url, "k"))).unwrap();
    hooks.emit("bug.filed", serde_json::json!({ "log": "x".repeat(2000) }));
    hooks.emit("bug.filed", serde_json::json!({ "log": "short" }));
    assert!(hooks.wait_idle(Duration::from_secs(5)));
    let got = rx.got();
    assert_eq!(got.len(), 1);
    assert!(got[0].body.contains("short"));
}

#[test]
fn an_unsigned_target_gets_no_signature_but_its_delivery_id() {
    let (signed, open) = (Receiver::start([]), Receiver::start([Answer::Status(503)]));
    let hooks = Hooks::new(
        config(None)
            .target(Target::new(&signed.url, "k"))
            .target(Target::unsigned(&open.url).events(["bug.filed"])),
    )
    .unwrap();
    hooks.emit("bug.filed", serde_json::json!({ "title": "x" }));
    let got = open.wait_for(2);
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    for g in &got {
        assert_eq!(g.header("x-signature"), "", "unsigned");
        assert_eq!(g.header("x-timestamp"), "");
        assert_eq!(g.header("x-hook-event"), "bug.filed");
    }
    assert_eq!(got[0].header("x-hook-delivery"), got[1].header("x-hook-delivery"), "kept on retry");
    assert_eq!(got[0].header("x-hook-delivery").len(), 36);
    check_signed(&signed.wait_for(1)[0], "k");
}

fn events(got: &[Got]) -> Vec<String> {
    got.iter().map(|g| g.header("x-hook-event").to_owned()).collect()
}

#[test]
fn a_sequence_is_delivered_in_order_and_holds_up_nothing_else() {
    let rx = Receiver::start([Answer::Status(503), Answer::Status(503)]);
    let hooks = Hooks::new(config(None).target(Target::new(&rx.url, "k"))).unwrap();
    hooks.emit_in("s1", "jam.shared", serde_json::json!({ "session": "s1" }));
    hooks.emit_in("s1", "jam.ended", serde_json::json!({ "session": "s1" }));
    // The first attempt failed: what is in no sequence goes meanwhile.
    rx.wait_for(1);
    hooks.emit("app.started", serde_json::json!({}));
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    let got = events(&rx.got());
    let shared_ok = got.iter().rposition(|e| e == "jam.shared").unwrap();
    let ended = got.iter().position(|e| e == "jam.ended").unwrap();
    assert!(shared_ok < ended, "jam.ended only after jam.shared went: {got:?}");
    let other = got.iter().position(|e| e == "app.started").unwrap();
    assert!(other < shared_ok, "app.started didn't wait on the sequence: {got:?}");
    assert_eq!(got.iter().filter(|e| *e == "jam.ended").count(), 1, "{got:?}");
    let s = &hooks.status().targets[0];
    assert_eq!((s.delivered, s.dropped), (3, 0));
}

#[test]
fn a_sequence_given_up_on_sends_nothing_more() {
    let rx = Receiver::start([Answer::Status(401)]);
    let hooks = Hooks::new(config(None).target(Target::new(&rx.url, "k"))).unwrap();
    hooks.emit_in("s1", "jam.shared", serde_json::json!({}));
    hooks.emit_in("s1", "jam.ended", serde_json::json!({}));
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    // Emitted in it later: dropped too. Another sequence is untouched.
    hooks.emit_in("s1", "jam.ended", serde_json::json!({}));
    hooks.emit_in("s2", "jam.shared", serde_json::json!({}));
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(events(&rx.got()), ["jam.shared", "jam.shared"]);
    let s = &hooks.status().targets[0];
    assert_eq!((s.delivered, s.dropped), (1, 2), "the late one was never queued");
}

#[test]
fn a_sequence_too_old_goes_whole() {
    let url = Receiver::down().url;
    let retry = Retry { max_age: Duration::from_secs(1), ..fast() };
    let hooks = Hooks::new(config(None).retry(retry).target(Target::new(&url, "k"))).unwrap();
    hooks.emit_in("s1", "jam.shared", serde_json::json!({}));
    hooks.emit_in("s1", "jam.ended", serde_json::json!({}));
    assert!(hooks.wait_idle(Duration::from_secs(10)), "given up on");
    let s = &hooks.status().targets[0];
    assert_eq!((s.delivered, s.dropped), (0, 2));
}

#[test]
fn a_sequence_keeps_its_order_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = dir.path().join("hooks-outbox.json");
    let rx = Receiver::down();
    let url = rx.url.clone();
    {
        let hooks =
            Hooks::new(config(Some(outbox.clone())).target(Target::new(&url, "k"))).unwrap();
        hooks.emit_in("s1", "jam.shared", serde_json::json!({}));
        hooks.emit_in("s1", "jam.ended", serde_json::json!({}));
        // The first attempt can fail before the worker has taken in
        // jam.ended: wait for both.
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let s = hooks.status();
            if s.pending == 2 && s.targets.first().is_some_and(|t| t.last_error.is_some()) {
                break;
            }
            assert!(Instant::now() < until, "an attempt failed, both pending: {s:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    std::thread::sleep(Duration::from_millis(200));
    // jam.ended was never tried, so it is due now; jam.shared only after its backoff.
    rx.up();
    let hooks = Hooks::new(config(Some(outbox)).target(Target::new(&url, "k"))).unwrap();
    rx.wait_for(2);
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    assert_eq!(events(&rx.got()), ["jam.shared", "jam.ended"]);
}

/// The one delivery of `id`'s event, once it is no longer pending.
fn ended(hooks: &Hooks, id: &EventId) -> DeliveryState {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let d = hooks.deliveries(id).expect("known");
        if let [one] = d.as_slice() {
            if !one.state.pending() {
                return one.state.clone();
            }
        }
        assert!(Instant::now() < until, "still pending: {d:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn each_event_reads_its_own_outcome() {
    let rx = Receiver::start([Answer::Status(200), Answer::Refuse(401, "unauthorized")]);
    let hooks = Hooks::new(config(None).target(Target::new(&rx.url, "k"))).unwrap();
    let first = hooks.emit("jam.shared", serde_json::json!({ "n": 1 })).unwrap();
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    let second = hooks.emit("jam.shared", serde_json::json!({ "n": 2 })).unwrap();
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    let got = rx.wait_for(2);
    assert!(matches!(ended(&hooks, &first), DeliveryState::Delivered { attempts: 1, .. }));
    let d = &hooks.deliveries(&first).unwrap()[0];
    assert_eq!(
        (d.url.as_str(), d.id.as_str()),
        (rx.url.as_str(), got[0].header("x-hook-delivery"))
    );
    match ended(&hooks, &second) {
        DeliveryState::Dropped { reason, attempts: 1, .. } => {
            assert_eq!(
                reason,
                DropReason::Refused { status: 401, error: Some("unauthorized".into()) }
            );
            assert_eq!(reason.to_string(), "refused: HTTP 401 (unauthorized)");
        }
        other => panic!("{other:?}"),
    }
    // The first still reads delivered, though the target's last word is a refusal.
    assert!(matches!(ended(&hooks, &first), DeliveryState::Delivered { .. }));
    assert!(hooks.emit("Not An Event", serde_json::json!({})).is_none());
    let none = Hooks::new(config(None)).unwrap();
    let id = none.emit("app.started", serde_json::json!({})).unwrap();
    assert!(none.wait_idle(Duration::from_secs(10)));
    assert_eq!(none.deliveries(&id), Some(vec![]), "no target wanted it");
}

#[test]
fn a_pending_event_keeps_its_id_and_state_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = dir.path().join("hooks-outbox.json");
    let url = Receiver::down().url;
    let rx = Receiver::start([]);
    let (kept, done) = {
        let hooks = Hooks::new(
            config(Some(outbox.clone()))
                .retry(Retry { first: Duration::from_secs(60), ..fast() })
                .targets(vec![
                    Target::new(&url, "k").events(["jam.*"]),
                    Target::new(&rx.url, "k").events(["app.*"]),
                ]),
        )
        .unwrap();
        let id = hooks.emit("jam.shared", serde_json::json!({})).unwrap();
        let done = hooks.emit("app.started", serde_json::json!({})).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let d = hooks.deliveries(&id).unwrap();
            if let [one] = d.as_slice() {
                if let DeliveryState::Pending {
                    attempts: 1,
                    last_error: Some(_),
                    next_try: Some(_),
                } = &one.state
                {
                    break;
                }
            }
            assert!(Instant::now() < until, "{d:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(ended(&hooks, &done), DeliveryState::Delivered { .. }));
        // What an app keeps of it.
        (serde_json::to_string(&id).unwrap(), done)
    };
    std::thread::sleep(Duration::from_millis(200));
    let id: EventId = serde_json::from_str(&kept).unwrap();
    let hooks = Hooks::new(
        config(Some(outbox)).retry(Retry { first: Duration::from_secs(60), ..fast() }).targets(
            vec![
                Target::new(&url, "k").events(["jam.*"]),
                Target::new(&rx.url, "k").events(["app.*"]),
            ],
        ),
    )
    .unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while hooks.deliveries(&id).is_none() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    match &hooks.deliveries(&id).unwrap()[..] {
        [d] => assert!(
            matches!(&d.state, DeliveryState::Pending { attempts: 1, last_error: Some(_), .. }),
            "{d:?}"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(hooks.deliveries(&done), None, "ended before the restart: not known");
}

#[test]
fn a_sequence_given_up_on_says_why_for_each_delivery() {
    let rx = Receiver::start([Answer::Refuse(403, "not a member")]);
    let hooks = Hooks::new(config(None).target(Target::new(&rx.url, "k"))).unwrap();
    let head = hooks.emit_in("s1", "jam.shared", serde_json::json!({})).unwrap();
    let rest = hooks.emit_in("s1", "jam.ended", serde_json::json!({})).unwrap();
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    let late = hooks.emit_in("s1", "jam.ended", serde_json::json!({})).unwrap();
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    let refused = DropReason::Refused { status: 403, error: Some("not a member".into()) };
    let after = DropReason::Sequence { cause: Box::new(refused.clone()) };
    for (id, want) in [(&head, &refused), (&rest, &after), (&late, &after)] {
        match ended(&hooks, id) {
            DeliveryState::Dropped { reason, .. } => assert_eq!(&reason, want),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        after.to_string(),
        "not sent: an earlier event in its sequence was refused: HTTP 403 (not a member)"
    );
    assert_eq!(rx.got().len(), 1);
}

#[test]
fn only_the_newest_ended_outcomes_are_kept() {
    let rx = Receiver::start([]);
    let hooks = Hooks::new(config(None).max_pending(2).target(Target::new(&rx.url, "k"))).unwrap();
    let ids: Vec<EventId> = (0..3)
        .map(|n| {
            let id = hooks.emit("app.started", serde_json::json!({ "n": n })).unwrap();
            assert!(hooks.wait_idle(Duration::from_secs(10)));
            ended(&hooks, &id);
            id
        })
        .collect();
    assert_eq!(hooks.deliveries(&ids[0]), None, "the oldest forgotten");
    assert!(hooks.deliveries(&ids[1]).is_some() && hooks.deliveries(&ids[2]).is_some());
}
