//! Linked devices against a receiver on loopback that keeps Lumina's
//! contract: a code swapped for a device id and key, once; refusals with
//! their reasons; an unlink taken only when signed by the device; and a
//! device target whose every attempt, retries included, names the device
//! and is signed with its key.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use simply_hooks::device::{self, LinkError, LinkRequest};
use simply_hooks::{sign, Config, Hooks, Retry, Secret, Target};

const DEVICE: &str = "0123456789abcdef0123456789abcdef";
const KEY: &str = "3167aacb872b76d2ce93f55c1ef7a589517f37241fc74357e59ff4c7a921ba4b";

#[derive(Clone, Debug)]
struct Got {
    path: String,
    headers: BTreeMap<String, String>,
    body: String,
}

/// A fake receiver: answers each request from `script` (status, extra
/// header lines, body), then by the contract: a link of `K7Q49X2MP1RT`
/// once, unlinks and hooks signed as [`DEVICE`] with [`KEY`].
fn receiver(script: Vec<(u16, &'static str, &'static str)>) -> (String, Arc<Mutex<Vec<Got>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let got: Arc<Mutex<Vec<Got>>> = Arc::default();
    let keep = got.clone();
    let mut script: VecDeque<_> = script.into();
    std::thread::spawn(move || {
        let mut used = false;
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let path = line.split(' ').nth(1).unwrap_or("").to_owned();
            let mut headers = BTreeMap::new();
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                let Some((k, v)) = h.trim_end().split_once(':') else { break };
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
            }
            let mut body = vec![0; headers.get("content-length").map_or(0, |n| n.parse().unwrap())];
            r.read_exact(&mut body).unwrap();
            let body = String::from_utf8(body).unwrap();
            let signed = || {
                let ts = headers.get("x-timestamp").and_then(|t| t.parse().ok()).unwrap_or(0);
                headers.get("x-hook-device").map(String::as_str) == Some(DEVICE)
                    && sign::verify(
                        KEY.as_bytes(),
                        ts,
                        body.as_bytes(),
                        headers.get("x-signature").map_or("", |s| s),
                    )
            };
            let (code, extra, answer): (u16, String, String) = match script.pop_front() {
                Some((c, e, b)) => (c, e.to_owned(), b.to_owned()),
                None if path == "/devices/v1/link" => {
                    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                    if v["code"] == "K7Q49X2MP1RT" && v["app"] == "app" && !used {
                        used = true;
                        let a = serde_json::json!({ "device_id": DEVICE, "key": KEY, "member": { "id": "4242", "name": "ana" }, "server": "home" });
                        (200, String::new(), a.to_string())
                    } else {
                        (401, String::new(), r#"{"error":"code not valid"}"#.into())
                    }
                }
                None if signed() => (200, String::new(), r#"{"ok":true}"#.into()),
                None => (401, String::new(), r#"{"error":"device not linked"}"#.into()),
            };
            keep.lock().unwrap().push(Got { path, headers, body });
            let _ = write!(
                stream,
                "HTTP/1.1 {code} X\r\n{extra}Content-Length: {}\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    (base, got)
}

fn wait_for(got: &Mutex<Vec<Got>>, n: usize) -> Vec<Got> {
    let until = Instant::now() + Duration::from_secs(10);
    while got.lock().unwrap().len() < n {
        assert!(Instant::now() < until, "waited for {n} requests");
        std::thread::sleep(Duration::from_millis(10));
    }
    got.lock().unwrap().clone()
}

fn req(code: &str) -> LinkRequest {
    LinkRequest {
        code: code.into(),
        app: "app".into(),
        label: "studio".into(),
        version: "1.2.3".into(),
    }
}

const T: Duration = Duration::from_secs(5);

#[test]
fn a_code_links_once_and_the_key_comes_back() {
    let (base, got) = receiver(vec![]);
    let l = device::link(&base, &req("k7q4-9x2m-plrt"), T).unwrap();
    assert_eq!(
        (
            l.device_id.as_str(),
            l.key.expose(),
            l.member_id.as_str(),
            l.member_name.as_str(),
            l.server.as_str()
        ),
        (DEVICE, KEY, "4242", "ana", "home")
    );
    let g = got.lock().unwrap()[0].clone();
    assert_eq!(g.path, "/devices/v1/link");
    assert_eq!(g.headers["content-type"], "application/json");
    assert_eq!(g.headers["user-agent"], "app/1.2.3");
    let sent: serde_json::Value = serde_json::from_str(&g.body).unwrap();
    assert_eq!(
        sent,
        serde_json::json!({ "code": "K7Q49X2MP1RT", "app": "app", "label": "studio", "version": "1.2.3" })
    );
    let again = device::link(&base, &req("K7Q4-9X2M-PLRT"), T).unwrap_err();
    assert_eq!(
        again,
        LinkError::Refused { status: 401, error: Some("code not valid".into()), retry_after: None },
        "once"
    );
    assert_eq!(device::link(&base, &req("hello"), T).unwrap_err(), LinkError::NotACode);
    assert_eq!(got.lock().unwrap().len(), 2, "nothing sent for what isn't a code");
}

#[test]
fn refusals_carry_their_reason_and_wait() {
    let (base, _got) = receiver(vec![
        (429, "Retry-After: 30\r\n", r#"{"error":"too many requests"}"#),
        (403, "", r#"{"error":"not a member"}"#),
        (200, "", r#"{"device_id":"0123","key":"x"}"#),
    ]);
    assert_eq!(
        device::link(&base, &req("K7Q4-9X2M-PLRT"), T).unwrap_err(),
        LinkError::Refused {
            status: 429,
            error: Some("too many requests".into()),
            retry_after: Some(30)
        }
    );
    assert_eq!(
        device::link(&base, &req("K7Q4-9X2M-PLRT"), T).unwrap_err().to_string(),
        "refused: HTTP 403 (not a member)"
    );
    assert!(matches!(device::link(&base, &req("K7Q4-9X2M-PLRT"), T), Err(LinkError::BadAnswer(_))));
    assert!(
        matches!(
            device::link("http://example.com", &req("K7Q4-9X2M-PLRT"), T),
            Err(LinkError::Unreachable(_))
        ),
        "http:// only to loopback"
    );
}

#[test]
fn an_unlink_is_signed_by_the_device() {
    let (base, got) = receiver(vec![]);
    device::unlink(&base, DEVICE, &Secret::new(KEY), "app/1.2.3", T).unwrap();
    let g = got.lock().unwrap()[0].clone();
    assert_eq!(
        (g.path.as_str(), g.body.as_str(), g.headers["x-hook-device"].as_str()),
        ("/devices/v1/unlink", "{}", DEVICE)
    );
    assert_eq!(g.headers["x-hook-delivery"].len(), 36);
    let wrong =
        device::unlink(&base, DEVICE, &Secret::new("not the key"), "app/1.2.3", T).unwrap_err();
    assert_eq!(wrong.to_string(), "refused: HTTP 401 (device not linked)");
}

#[test]
fn a_device_target_names_its_device_on_every_attempt() {
    let (base, got) = receiver(vec![(503, "", ""), (401, "", r#"{"error":"device not linked"}"#)]);
    let url = format!("{base}/hooks/home/jams");
    let fast = Retry {
        first: Duration::from_millis(100),
        max: Duration::from_millis(400),
        max_age: Duration::from_secs(3600),
    };
    let hooks =
        Hooks::new(Config::new(None, "app/1").retry(fast).timeout(T).target(Target::device(
            &url,
            DEVICE,
            &Secret::new(KEY),
        )))
        .unwrap();
    hooks.emit("jam.shared", serde_json::json!({ "session": "s1" }));
    let all = wait_for(&got, 2);
    assert!(hooks.wait_idle(T));
    for g in &all {
        assert_eq!(g.headers["x-hook-device"], DEVICE, "a retry names the device too");
        let ts: u64 = g.headers["x-timestamp"].parse().unwrap();
        assert!(sign::verify(KEY.as_bytes(), ts, g.body.as_bytes(), &g.headers["x-signature"]));
    }
    assert_eq!(all[0].headers["x-hook-delivery"], all[1].headers["x-hook-delivery"]);
    let s = &hooks.status().targets[0];
    assert_eq!(
        (s.dropped, s.last_error.as_deref()),
        (1, Some("refused: HTTP 401 (device not linked)")),
        "refused, and why"
    );
}
