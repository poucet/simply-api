//! Deliveries against a receiver on loopback: what is sent and how it is
//! signed, retries, the outbox across a restart, the allowlist, and an
//! `emit` that returns while a delivery hangs.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use simply_hooks::{sign, Config, Hooks, Retry, Target};

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
    /// Read the request, never answer.
    Hang,
}

/// A receiver: answers from a script (then 200), records every request.
#[derive(Clone)]
struct Receiver {
    url: String,
    got: Arc<Mutex<Vec<Got>>>,
    script: Arc<Mutex<VecDeque<Answer>>>,
}

impl Receiver {
    fn start(script: impl IntoIterator<Item = Answer>) -> Receiver {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        Receiver::on(listener, script)
    }

    fn on(listener: TcpListener, script: impl IntoIterator<Item = Answer>) -> Receiver {
        let url = format!("http://127.0.0.1:{}/hooks/in", listener.local_addr().unwrap().port());
        let r = Receiver {
            url,
            got: Arc::default(),
            script: Arc::new(Mutex::new(script.into_iter().collect())),
        };
        let me = r.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
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
                        Answer::Hang => std::thread::sleep(Duration::from_secs(30)),
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
    let ts: u64 = g.header("x-simply-timestamp").parse().unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(now.abs_diff(ts) <= 5, "the timestamp is this attempt's");
    let sig = g.header("x-simply-signature");
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
    assert_eq!(g.header("x-simply-event"), "app.started");
    assert_eq!(g.header("user-agent"), "simply-hooks-test/1");
    let id = g.header("x-simply-delivery");
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
    let ids: Vec<&str> = got.iter().map(|g| g.header("x-simply-delivery")).collect();
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
    // A port nobody listens on yet.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}/in", listener.local_addr().unwrap().port());
    drop(listener);
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
    let rx = Receiver::on(TcpListener::bind(url[7..].split('/').next().unwrap()).unwrap(), []);
    let hooks = Hooks::new(config(Some(outbox.clone())).target(Target::new(&url, "k"))).unwrap();
    let got = rx.wait_for(1);
    assert!(hooks.wait_idle(Duration::from_secs(10)));
    assert_eq!(got[0].header("x-simply-delivery"), id, "the same delivery");
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
            r.got().iter().map(|g| g.header("x-simply-event").to_owned()).collect();
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
