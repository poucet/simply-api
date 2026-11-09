//! The app side end to end, against a scripted Lumina on loopback.

mod common;

use common::{relay, until, FakeLumina, Studio};
use serde_json::{json, Value};
use simply_relay::wire::{AppFrame, Auth, RefusedReason};
use simply_relay::{PairError, Status, Verdict};

fn hello_auth(frame: &AppFrame) -> &Auth {
    match frame {
        AppFrame::Hello(hello) => &hello.auth,
        other => panic!("expected a hello, got {other:?}"),
    }
}

fn stored(dir: &std::path::Path) -> toml::Table {
    toml::from_str(&std::fs::read_to_string(dir.join("lumina.toml")).unwrap()).unwrap()
}

#[cfg(unix)]
fn mode(dir: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(dir.join("lumina.toml")).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn pairing_links_the_device_and_stores_its_token_privately() {
    let dir = tempfile::tempdir().unwrap();
    let lumina = FakeLumina::start().await;
    let studio = Studio::default();
    let app = relay(dir.path(), &lumina).spawn({
        let studio = studio.clone();
        move || studio.clone()
    });
    let app = app.unwrap();
    assert_eq!(app.status(), Status::Unpaired);

    let pairing = tokio::spawn({
        let app = app.clone();
        async move { app.pair(" K7Q4-9X2M-PLRT ").await }
    });
    let (first, conn) = lumina.accept().await;
    let AppFrame::Hello(hello) = &first else { panic!("hello") };
    assert_eq!(hello.auth, Auth::Pair { pair: "K7Q4-9X2M-PLRT".into() });
    assert_eq!((hello.v, hello.app.as_str(), hello.version.as_str()), (1, "studio", "1.2.3"));
    assert_eq!(hello.label, "test-mac");
    assert!(!hello.consent.read_only, "everything is off by default");
    assert_eq!(hello.consent.actions.get("session"), Some(&false));

    let mcp = conn.welcome("d-1", "token-1", Some("chris")).await;
    let linked = pairing.await.unwrap().expect("paired");
    assert_eq!((linked.device_id.as_str(), linked.member.as_deref()), ("d-1", Some("chris")));
    until(&app, |s| matches!(s, Status::Connected { .. })).await;

    let file = stored(dir.path());
    assert_eq!(file["device_id"].as_str(), Some("d-1"));
    assert_eq!(file["token"].as_str(), Some("token-1"));
    assert_eq!(file["linked_to"].as_str(), Some("chris"));
    assert_eq!(file["endpoint"].as_str(), Some(lumina.url.as_str()));
    #[cfg(unix)]
    assert_eq!(mode(dir.path()), 0o600);

    // With every switch off, Lumina sees nothing.
    assert!(mcp.tool_names().await.is_empty());
    app.shutdown().await;
}

#[tokio::test]
async fn an_expired_code_links_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let lumina = FakeLumina::start().await;
    let app = relay(dir.path(), &lumina).spawn(Studio::default).unwrap();

    let pairing = tokio::spawn({
        let app = app.clone();
        async move { app.pair("OLD-CODE").await }
    });
    let (first, conn) = lumina.accept().await;
    assert!(matches!(hello_auth(&first), Auth::Pair { .. }));
    conn.refuse(RefusedReason::Code, "That code has expired.").await;

    assert_eq!(pairing.await.unwrap(), Err(PairError::Code("That code has expired.".into())));
    until(&app, |s| *s == Status::Unpaired).await;
    assert!(!dir.path().join("lumina.toml").exists(), "nothing is stored");
    app.shutdown().await;
}

#[tokio::test]
async fn a_silent_lumina_fails_the_pairing_instead_of_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let lumina = FakeLumina::start().await;
    let app = relay(dir.path(), &lumina)
        .handshake_timeout(std::time::Duration::from_millis(200))
        .spawn(Studio::default)
        .unwrap();
    let pairing = tokio::spawn({
        let app = app.clone();
        async move { app.pair("CODE").await }
    });
    let (_first, _conn) = lumina.accept().await; // never answers
    assert!(matches!(pairing.await.unwrap(), Err(PairError::Connect(_))));
    app.shutdown().await;
}

#[tokio::test]
async fn the_token_rotates_on_every_connect_and_the_app_reconnects() {
    let dir = tempfile::tempdir().unwrap();
    let lumina = FakeLumina::start().await;
    let app = relay(dir.path(), &lumina).spawn(Studio::default).unwrap();

    let pairing = tokio::spawn({
        let app = app.clone();
        async move { app.pair("CODE").await }
    });
    let (_, conn) = lumina.accept().await;
    let mcp = conn.welcome("d-1", "token-1", Some("chris")).await;
    pairing.await.unwrap().unwrap();

    // Lumina goes away; the app dials again with the token it was given.
    mcp.client.cancel().await.unwrap();
    let (first, conn) = lumina.accept().await;
    assert_eq!(
        hello_auth(&first),
        &Auth::Token { device_id: "d-1".into(), token: "token-1".into() }
    );
    let mcp = conn.welcome("d-1", "token-2", None).await;
    until(&app, |s| matches!(s, Status::Connected { .. })).await;
    let file = stored(dir.path());
    assert_eq!(file["token"].as_str(), Some("token-2"), "the new token replaced the old");
    assert_eq!(file["linked_to"].as_str(), Some("chris"), "a welcome without a name keeps it");
    #[cfg(unix)]
    assert_eq!(mode(dir.path()), 0o600, "still private after the rewrite");

    // A connection that fails outright is retried, with the newest token.
    mcp.client.cancel().await.unwrap();
    let (_, conn) = lumina.accept().await;
    drop(conn);
    until(&app, |s| matches!(s, Status::Retrying { .. })).await;
    let (first, conn) = lumina.accept().await;
    assert_eq!(
        hello_auth(&first),
        &Auth::Token { device_id: "d-1".into(), token: "token-2".into() }
    );

    // Revoked: the app forgets its link and stops dialling.
    conn.refuse(RefusedReason::Unlinked, "This device was unlinked.").await;
    until(&app, |s| matches!(s, Status::Unlinked { .. })).await;
    let file = stored(dir.path());
    assert!(file.get("token").is_none() && file.get("device_id").is_none(), "{file:?}");
    app.shutdown().await;
}

#[tokio::test]
async fn a_restarted_app_dials_with_its_stored_token_and_disconnect_unlinks() {
    let dir = tempfile::tempdir().unwrap();
    let lumina = FakeLumina::start().await;
    std::fs::write(
        dir.path().join("lumina.toml"),
        format!("endpoint = \"{}\"\ndevice_id = \"d-9\"\ntoken = \"kept\"\n", lumina.url),
    )
    .unwrap();
    let app = relay(dir.path(), &lumina).spawn(Studio::default).unwrap();
    let (first, conn) = lumina.accept().await;
    assert_eq!(hello_auth(&first), &Auth::Token { device_id: "d-9".into(), token: "kept".into() });
    let _mcp = conn.welcome("d-9", "fresh", None).await;
    until(&app, |s| matches!(s, Status::Connected { .. })).await;

    let disconnect = tokio::spawn({
        let app = app.clone();
        async move { app.disconnect().await }
    });
    let (first, conn) = lumina.accept().await;
    let AppFrame::Unlink(unlink) = first else { panic!("an unlink frame") };
    assert_eq!((unlink.device_id.as_str(), unlink.token.as_str()), ("d-9", "fresh"));
    conn.refuse(RefusedReason::Unlinked, "Unlinked.").await;
    disconnect.await.unwrap();
    assert_eq!(app.status(), Status::Unpaired);
    assert!(stored(dir.path()).get("token").is_none());
    app.shutdown().await;
}

/// Pair and return a connected Lumina client.
async fn paired(
    dir: &std::path::Path,
    lumina: &FakeLumina,
    builder: impl FnOnce(simply_relay::Builder) -> simply_relay::Builder,
    studio: &Studio,
) -> (simply_relay::Relay, common::Mcp) {
    let studio = studio.clone();
    let app = builder(relay(dir, lumina)).spawn(move || studio.clone()).unwrap();
    let pairing = tokio::spawn({
        let app = app.clone();
        async move { app.pair("CODE").await }
    });
    let (_, conn) = lumina.accept().await;
    let mcp = conn.welcome("d-1", "t-1", None).await;
    pairing.await.unwrap().unwrap();
    until(&app, |s| matches!(s, Status::Connected { .. })).await;
    (app, mcp)
}

#[tokio::test]
async fn a_tool_the_switches_do_not_allow_is_refused_before_the_app_sees_it() {
    let dir = tempfile::tempdir().unwrap();
    let lumina = FakeLumina::start().await;
    let studio = Studio::default();
    let (app, mut mcp) = paired(dir.path(), &lumina, |b| b, &studio).await;

    let refused = mcp.call("levels", json!({}), "bug studio-0042").await.unwrap_err();
    assert_eq!(refused.code.0, -32602);
    assert_eq!(refused.data, Some(json!({"refused": "switched_off", "class": "read_only"})));
    assert!(
        refused.message.contains("diagnostics are switched off in studio"),
        "{}",
        refused.message
    );
    assert!(studio.reached().is_empty(), "the handler never saw it");

    // Diagnostics on: Lumina is told to re-list, and read-only tools answer.
    app.set_read_only(true).unwrap();
    tokio::time::timeout(common::WAIT, mcp.list_changed.recv()).await.unwrap().unwrap();
    let mut names = mcp.tool_names().await;
    names.sort();
    assert_eq!(
        names,
        [
            "echo",
            "levels",
            "lumina_about",
            "lumina_config",
            "lumina_errors",
            "lumina_log",
            "lumina_report"
        ]
    );
    let levels = mcp.call("levels", json!({}), "bug studio-0042").await.unwrap();
    assert_eq!(levels.structured_content, Some(json!({"rate": 48000, "peaks": [-6.0]})));

    // An action stays off under the diagnostics switch; an undeclared tool
    // is never offered, whatever is on.
    let action = mcp.call("open_session", json!({}), "jam #jam-chris").await.unwrap_err();
    assert_eq!(action.data, Some(json!({"refused": "switched_off", "class": "session"})));
    let wipe = mcp.call("wipe", json!({}), "?").await.unwrap_err();
    assert_eq!(wipe.data, Some(json!({"refused": "not_exposed", "class": null})));

    app.set_action("session", true).unwrap();
    tokio::time::timeout(common::WAIT, mcp.list_changed.recv()).await.unwrap().unwrap();
    assert!(mcp.tool_names().await.contains(&"open_session".to_owned()));
    mcp.call("open_session", json!({}), "jam #jam-chris").await.unwrap();
    assert!(mcp.call("wipe", json!({}), "?").await.is_err());
    assert_eq!(studio.reached(), ["levels", "open_session"]);
    assert!(app.set_action("nonsense", true).is_err());

    // The member's log has every call, with Lumina's stated purpose.
    let calls = app.recent_calls(50);
    let summary: Vec<(&str, &str, Verdict, Option<&str>)> = calls
        .iter()
        .map(|c| (c.tool.as_str(), c.class.as_str(), c.verdict, c.purpose.as_deref()))
        .collect();
    assert_eq!(
        summary,
        [
            ("levels", "read_only", Verdict::Refused, Some("bug studio-0042")),
            ("levels", "read_only", Verdict::Allowed, Some("bug studio-0042")),
            ("open_session", "session", Verdict::Refused, Some("jam #jam-chris")),
            ("wipe", "none", Verdict::Refused, Some("?")),
            ("open_session", "session", Verdict::Allowed, Some("jam #jam-chris")),
            ("wipe", "none", Verdict::Refused, Some("?")),
        ]
    );
    assert!(
        !std::fs::read_to_string(app.call_log_path()).unwrap().contains("text"),
        "no arguments"
    );

    // The switches survive a restart.
    app.shutdown().await;
    let file = stored(dir.path());
    assert_eq!(file["consent"]["read_only"].as_bool(), Some(true));
    assert_eq!(file["consent"]["actions"]["session"].as_bool(), Some(true));
}

#[tokio::test]
async fn diagnostics_redact_the_config_and_report_everything() {
    use tracing_subscriber::layer::SubscriberExt;
    let subscriber = tracing_subscriber::registry().with(simply_relay::log_layer());
    let _guard = tracing::subscriber::set_default(subscriber);

    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/someone".into());
    let dir = tempfile::tempdir().unwrap();
    let lumina = FakeLumina::start().await;
    let studio = Studio::default();
    let config_home = home.clone();
    let (app, mcp) = paired(
        dir.path(),
        &lumina,
        move |b| {
            b.build("abc123")
                .config(move || {
                    json!({
                        "audio": {"rate": 48000, "device": "MOTU M4", "keyboard": "on"},
                        "api_key": "sk-live-1",
                        "cloud": {"accessToken": "tok", "password": "hunter2", "author": "chris"},
                        "secrets": {"github": "ghp_x", "list": ["a", "b"]},
                        "projects_dir": format!("{config_home}/Music/projects"),
                        "studio_name": "Chris's attic",
                    })
                })
                .redactor(|config| {
                    config["studio_name"] = Value::String("[hidden by the app]".into());
                })
        },
        &studio,
    )
    .await;
    app.set_read_only(true).unwrap();

    let config =
        mcp.call("lumina_config", json!({}), "bug").await.unwrap().structured_content.unwrap();
    assert_eq!(
        config["config"],
        json!({
            "audio": {"rate": 48000, "device": "MOTU M4", "keyboard": "on"},
            "api_key": "[redacted]",
            "cloud": {"accessToken": "[redacted]", "password": "[redacted]", "author": "chris"},
            "secrets": {"github": "[redacted]", "list": ["[redacted]", "[redacted]"]},
            "projects_dir": "~/Music/projects",
            "studio_name": "[hidden by the app]",
        })
    );

    let marker = "relay-diag-marker-7f3a";
    tracing::info!("opened {home}/Music/{marker}.wav");
    tracing::error!("{marker}: device lost");
    let log = mcp
        .call("lumina_log", json!({"lines": 500}), "bug")
        .await
        .unwrap()
        .structured_content
        .unwrap();
    let lines = log["lines"].as_array().unwrap();
    let opened =
        lines.iter().find(|l| l["message"].as_str().unwrap().contains(marker)).expect("logged");
    assert_eq!(opened["message"], format!("opened ~/Music/{marker}.wav"));
    let errors =
        mcp.call("lumina_errors", json!({}), "bug").await.unwrap().structured_content.unwrap();
    assert!(errors["errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l["level"] == "ERROR" && l["message"].as_str().unwrap().contains(marker)));

    let about =
        mcp.call("lumina_about", json!({}), "bug").await.unwrap().structured_content.unwrap();
    assert_eq!((about["app"].as_str(), about["version"].as_str()), (Some("studio"), Some("1.2.3")));
    assert_eq!(about["build"], "abc123");
    assert_eq!(about["os"], std::env::consts::OS);

    // The report runs `levels` (no argument) but not `echo` (one required).
    let report = mcp
        .call("lumina_report", json!({}), "bug studio-7")
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(report["checks"], json!({"levels": {"rate": 48000, "peaks": [-6.0]}}));
    assert_eq!(report["config"]["api_key"], "[redacted]");
    assert_eq!(report["about"]["app"], "studio");
    assert!(report["log"].is_array() && report["errors"].is_array());
    assert_eq!(studio.reached(), ["levels"]);
    let last = app.recent_calls(2);
    assert_eq!(
        last.iter().map(|c| (c.tool.as_str(), c.purpose.as_deref())).collect::<Vec<_>>(),
        [("lumina_report", Some("bug studio-7")), ("levels", Some("bug studio-7"))]
    );
    app.shutdown().await;
}
