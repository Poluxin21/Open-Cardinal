//! Process-level tests of a single daemon: the original behaviour, and one regression test
//! per reproduced vulnerability (PoC 1-4 in docs/SECURITY.md).

mod common;

use std::time::Duration;

use common::*;
use open_cardinal::control::protocol::{CliRequest, Envelope};
use serde_json::{Value, json};

fn legacy_spec() -> Spec {
    Spec::single().file("rules/default/default.lua", DEFAULT_RULE)
}

// ---------------------------------------------------------------------------------------
// behaviour that must stay exactly as it was
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn shipped_default_rule_decides_like_the_original() {
    let d = Daemon::start(legacy_spec()).await;
    let r = d.ping("Rocket_01", &[("fuel", "53"), ("altitude", "10")], None).await.unwrap();
    assert_eq!(r.r#type, 1, "SHUTDOWN");
    assert_eq!(r.command_name, "EMERGENCY_CUTOFF");
    assert_eq!(r.parameters["reason"], "Overheating");
    assert!(!r.trace_id.is_empty());

    let r = d.ping("Rocket_01", &[("fuel", "89")], None).await.unwrap();
    assert_eq!(r.r#type, 0, "IDLE");
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_specific_rules_replace_the_default_ones() {
    let d = Daemon::start(
        legacy_spec().file("rules/Special/a.lua", "return { action = 'RESTART', cmd_name = 'SPECIAL', priority = 5 }"),
    )
    .await;
    assert_eq!(d.ping("Special", &[("fuel", "1")], None).await.unwrap().command_name, "SPECIAL");
    assert_eq!(d.ping("Other", &[("fuel", "1")], None).await.unwrap().command_name, "EMERGENCY_CUTOFF");
    assert_eq!(d.ping("Other", &[("fuel", "100")], None).await.unwrap().command_name, "NO_ACTION");
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_http_endpoints_keep_their_shape() {
    let d = Daemon::start(legacy_spec()).await;
    tokio::time::sleep(Duration::from_millis(1200)).await; // first system sample
    let (code, _, body) = d.http_get("/metrics", None).await;
    assert_eq!(code, 200);
    let m: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(m["total_rules"], 1);
    assert_eq!(m["agents_detected"], 0);
    assert_eq!(m["connected_agents"], 0);

    let (code, _, body) = d.http_get("/info", None).await;
    assert_eq!(code, 200);
    let i: Value = serde_json::from_str(&body).unwrap();
    for k in ["kernel_version", "cpu_usage", "used_mem", "total_mem"] {
        assert!(i.get(k).is_some(), "/info lost the '{k}' field: {body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn heathcliff_force_and_revoke() {
    let d = Daemon::start(legacy_spec()).await;
    let msg = d.ok(Daemon::heathcliff("force", &["--agent", "Pump", "--force", "2"])).await;
    assert!(msg.contains("RESTART"), "{msg}");
    let r = d.ping("Pump", &[("fuel", "100")], None).await.unwrap();
    assert_eq!((r.r#type, r.command_name.as_str()), (2, "heathcliff"));
    // other agents are untouched
    assert_eq!(d.ping("Other", &[("fuel", "100")], None).await.unwrap().r#type, 0);

    d.ok(Daemon::heathcliff("revoke_force", &["--agent", "Pump"])).await;
    assert_eq!(d.ping("Pump", &[("fuel", "100")], None).await.unwrap().r#type, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn heathcliff_refuses_incomplete_commands_instead_of_silently_succeeding() {
    let d = Daemon::start(legacy_spec()).await;
    for args in [
        vec![],
        vec!["--agent", "X"],
        vec!["--force", "1"],
        vec!["--agent", "X", "--force", "9"],
        vec!["--agent", "X", "--force", "abc"],
    ] {
        let e = d.err(Daemon::heathcliff("force", &args)).await;
        assert!(e.contains("--agent") || e.contains("--force"), "{args:?} -> {e}");
    }
    // CUSTOM needs a command name, and expiry works
    assert!(d.err(Daemon::heathcliff("force", &["--agent", "X", "--force", "3"])).await.contains("--cmd"));
    d.ok(Daemon::heathcliff(
        "force",
        &["--agent", "X", "--force", "3", "--cmd", "OPEN_VALVE", "--param", "id=7", "--ttl", "1"],
    ))
    .await;
    let r = d.ping("X", &[], None).await.unwrap();
    assert_eq!((r.r#type, r.command_name.as_str(), r.parameters["id"].as_str()), (3, "OPEN_VALVE", "7"));
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(d.ping("X", &[("fuel", "100")], None).await.unwrap().r#type, 0, "the override expired");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_wiki_persistence_example_works_on_the_very_first_run() {
    // original: `redb_api.get` of a missing key panicked the worker and reset the stream
    let d = Daemon::start(Spec::single().file("rules/Victim/strikes.lua", STRIKES_RULE)).await;
    for expect_shutdown in [false, false, true, false] {
        let r = d.ping("Victim", &[("cpu_temp", "95")], None).await.unwrap();
        assert_eq!(r.r#type == 1, expect_shutdown);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn state_survives_a_restart() {
    let d = Daemon::start(Spec::single().file("rules/Victim/strikes.lua", STRIKES_RULE)).await;
    d.ping("Victim", &[("cpu_temp", "95")], None).await.unwrap();
    d.ping("Victim", &[("cpu_temp", "95")], None).await.unwrap();
    let spec = Spec { grpc: d.grpc, http: d.http, control_port: d.control.port(), ..Spec::single() };
    let (home, tmp) = d.into_home();
    let d2 = Daemon::start_in(spec, home, tmp).await;
    let r = d2.ping("Victim", &[("cpu_temp", "95")], None).await.unwrap();
    assert_eq!(r.command_name, "PERSISTENT_OVERHEAT", "the two strikes written before the restart still count");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_pulses_never_fail_and_the_counter_does_not_leak() {
    // original: with 4+ connections 84-94% of pulses failed with "Database already open",
    // and `connected_agents` leaked on every failure
    let d = Daemon::start(legacy_spec()).await;
    let mut tasks = Vec::new();
    for w in 0..32 {
        let mut c = d.grpc_client().await;
        tasks.push(tokio::spawn(async move {
            let mut errors = 0;
            for _ in 0..50 {
                if ping_with(&mut c, &format!("Par_{w}"), &[("fuel", "100")], None).await.is_err() {
                    errors += 1;
                }
            }
            errors
        }));
    }
    let mut errors = 0;
    for t in tasks {
        errors += t.await.unwrap();
    }
    assert_eq!(errors, 0);
    let status = d.data(CliRequest::Status).await;
    assert_eq!(status["active_connections"], 0);
    let stats = d.data(CliRequest::Stats).await;
    assert_eq!(stats["pulses"], 32 * 50);
}

#[tokio::test(flavor = "multi_thread")]
async fn rules_hot_reload_when_files_change() {
    let d = Daemon::start(legacy_spec()).await;
    assert_eq!(d.ping("New", &[("fuel", "100")], None).await.unwrap().r#type, 0);
    std::fs::create_dir_all(d.path("rules/New")).unwrap();
    std::fs::write(d.path("rules/New/r.lua"), "return { action = 'RESTART', cmd_name = 'HOT', priority = 1 }").unwrap();
    let reloaded = wait_until(Duration::from_secs(10), || async {
        d.ping("New", &[("fuel", "100")], None).await.unwrap().command_name == "HOT"
    })
    .await;
    assert!(reloaded, "the file watcher must pick up new rules (a regression here once dropped the watcher)");
    std::fs::write(d.path("rules/New/r.lua"), "return { action = 'RESTART', cmd_name = 'HOT2', priority = 1 }")
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || async {
            d.ping("New", &[("fuel", "100")], None).await.unwrap().command_name == "HOT2"
        })
        .await
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broken_rule_is_reported_and_does_not_stop_the_others() {
    let d = Daemon::start(
        legacy_spec()
            .file("rules/Agent/good.lua", "return { action = 'RESTART', cmd_name = 'OK' }")
            .file("rules/Agent/bad.lua", "if then"),
    )
    .await;
    assert_eq!(d.ping("Agent", &[], None).await.unwrap().command_name, "OK");
    let issues = d.data(CliRequest::Rules { command: "issues".into(), args: vec![] }).await;
    assert!(issues["issues"][0]["path"].as_str().unwrap().ends_with("bad.lua"), "{issues}");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_pulses_are_rejected_not_processed() {
    let d = Daemon::start(legacy_spec()).await;
    assert_eq!(d.ping("", &[], None).await.unwrap_err().code(), tonic::Code::InvalidArgument);
    assert_eq!(d.ping("a\nFAKE LOG LINE", &[], None).await.unwrap_err().code(), tonic::Code::InvalidArgument);
    assert_eq!(d.ping(&"x".repeat(300), &[], None).await.unwrap_err().code(), tonic::Code::InvalidArgument);
}

// ---------------------------------------------------------------------------------------
// security regressions (docs/SECURITY.md)
// ---------------------------------------------------------------------------------------

/// F-01 / PoC 2: the control plane accepted commands from any local process.
#[tokio::test(flavor = "multi_thread")]
async fn poc2_control_plane_refuses_unauthenticated_commands() {
    let mut d = Daemon::start(legacy_spec()).await;

    // exactly what the PoC sent against the original daemon
    let forced = raw_control(
        d.control,
        br#"{"Heathcliff":{"command":"force","args":["--agent","Healthy_Pump","--force","1"]}}"#,
    )
    .await;
    assert!(forced.contains("Error"), "{forced}");
    assert_eq!(d.ping("Healthy_Pump", &[("fuel", "100")], None).await.unwrap().r#type, 0, "no override was installed");

    let stop = raw_control(d.control, br#""Stop""#).await;
    assert!(stop.contains("Error"), "{stop}");
    // wrong token, empty token, oversized junk
    for env in [
        Envelope { token: "x".repeat(64), request: CliRequest::Stop },
        Envelope { token: String::new(), request: CliRequest::Stop },
    ] {
        assert!(raw_control(d.control, &serde_json::to_vec(&env).unwrap()).await.contains("authentication failed"));
    }
    assert!(raw_control(d.control, &vec![b'A'; 200_000]).await.contains("too large"));

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(d.is_running(), "an unauthenticated Stop must not stop the daemon");
    assert!(matches!(d.ctl(CliRequest::Status).await, open_cardinal::control::protocol::CliResponse::Data { .. }));
}

/// F-03 / PoC 3: a rule file ran `os.execute` as soon as it was written.
#[tokio::test(flavor = "multi_thread")]
async fn poc3_rules_cannot_run_operating_system_commands() {
    let marker = tempfile::tempdir().unwrap();
    let target = marker.path().join("pwned.txt");
    let cmd = if cfg!(windows) {
        format!("cmd /c echo pwned > \"{}\"", target.display())
    } else {
        format!("echo pwned > '{}'", target.display())
    };
    let rule = format!(
        "os.execute({cmd:?})\nio.open({:?}, 'w')\nreturn {{ action = 'RESTART' }}",
        target.display().to_string()
    );
    let d = Daemon::start(legacy_spec()).await;
    std::fs::create_dir_all(d.path("rules/Pwn")).unwrap();
    std::fs::write(d.path("rules/Pwn/x.lua"), rule).unwrap(); // the original executed this on file write
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_path_missing(&target);
    // and when the rule is actually evaluated
    let r = d.ping("Pwn", &[("fuel", "1")], None).await.unwrap();
    assert_eq!(r.r#type, 0, "the failing rule yields no decision");
    assert_path_missing(&target);
    let audit = d
        .data(CliRequest::Audit {
            command: "tail".into(),
            args: vec!["--agent".into(), "Pwn".into(), "--limit".into(), "1".into()],
        })
        .await;
    let text = audit.to_string();
    assert!(text.contains("nil value") || text.contains("attempt to index"), "the failure is audited: {text}");
}

/// F-04 / PoC 4: `while true do end` in a rule froze the whole daemon (22 pulses sufficed).
#[tokio::test(flavor = "multi_thread")]
async fn poc4_an_infinite_loop_rule_cannot_freeze_the_daemon() {
    let d = Daemon::start(legacy_spec().file("rules/Loop/loop.lua", "while true do end")).await;
    let mut tasks = Vec::new();
    for _ in 0..100 {
        let mut c = d.grpc_client().await;
        tasks.push(tokio::spawn(async move { ping_with(&mut c, "Loop", &[("fuel", "1")], None).await }));
    }
    // while the storm is processed, everything else keeps answering
    tokio::time::sleep(Duration::from_millis(300)).await;
    let status = tokio::time::timeout(Duration::from_secs(5), d.try_ctl(CliRequest::Status))
        .await
        .expect("control plane froze")
        .unwrap();
    assert!(matches!(status, open_cardinal::control::protocol::CliResponse::Data { .. }));
    let (code, _, _) =
        tokio::time::timeout(Duration::from_secs(5), d.http_get("/healthz", None)).await.expect("http froze");
    assert_eq!(code, 200);
    let other = tokio::time::timeout(Duration::from_secs(5), d.ping("Healthy", &[("fuel", "100")], None))
        .await
        .expect("gRPC froze")
        .unwrap();
    assert_eq!(other.r#type, 0);
    let (mut decided, mut refused) = (0, 0);
    for t in tasks {
        match t.await.unwrap() {
            Ok(r) => {
                assert_eq!(r.r#type, 0, "a rule that exhausts its budget yields no decision");
                decided += 1;
            }
            // beyond the tenant's concurrency the daemon refuses fast instead of queueing forever
            Err(e) => {
                assert_eq!(e.code(), tonic::Code::ResourceExhausted, "{e}");
                refused += 1;
            }
        }
    }
    assert_eq!(decided + refused, 100);
    assert!(decided > 0);
    assert_eq!(d.data(CliRequest::Status).await["active_connections"], 0);
}

/// F-02 / PoC 1: an unauthenticated peer could forge `agent_id` and reset another agent's state,
/// suppressing a safety shutdown.
#[tokio::test(flavor = "multi_thread")]
async fn poc1_agent_spoofing_is_blocked_once_keys_exist() {
    let d = Daemon::start(Spec::single().file("rules/Victim/strikes.lua", STRIKES_RULE)).await;
    // 1) control run: two hot readings
    d.ping("Victim", &[("cpu_temp", "95")], None).await.unwrap();
    // 2) the operator issues the fleet key: authentication switches on by itself
    let key = d
        .data(Daemon::tenant("issue-key", &["default", "--label", "fleet", "--agent-prefix", "Victim"]))
        .await["api_key"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(key.starts_with("ck_"));
    // only the hash is stored
    let stored = std::fs::read_to_string(d.path("config/tenants.json")).unwrap();
    assert!(!stored.contains(&key));

    // 3) the attacker (no key / wrong key / right key but another fleet)
    assert_eq!(d.ping("Victim", &[("cpu_temp", "20")], None).await.unwrap_err().code(), tonic::Code::Unauthenticated);
    assert_eq!(
        d.ping("Victim", &[("cpu_temp", "20")], Some("ck_wrong")).await.unwrap_err().code(),
        tonic::Code::Unauthenticated
    );
    assert_eq!(
        d.ping("OtherFleet_1", &[("cpu_temp", "20")], Some(&key)).await.unwrap_err().code(),
        tonic::Code::PermissionDenied
    );

    // 4) the real device keeps its counter: the next two hot readings complete the 3 strikes
    assert_eq!(d.ping("Victim", &[("cpu_temp", "95")], Some(&key)).await.unwrap().r#type, 0);
    assert_eq!(d.ping("Victim", &[("cpu_temp", "95")], Some(&key)).await.unwrap().command_name, "PERSISTENT_OVERHEAT");

    // the attack is visible in the audit trail
    let audit = d
        .data(CliRequest::Audit {
            command: "tail".into(),
            args: vec!["--event".into(), "rejected".into(), "--limit".into(), "10".into()],
        })
        .await;
    assert!(audit.to_string().contains("authentication failed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn secure_by_default_in_containers_and_when_exposed() {
    // a container runtime requires credentials from the start, even on loopback
    let d = Daemon::start(legacy_spec().env("CARDINAL_RUNTIME", "docker")).await;
    assert_eq!(d.ping("A", &[("fuel", "1")], None).await.unwrap_err().code(), tonic::Code::Unauthenticated);
    let key = d.data(Daemon::tenant("issue-key", &["default", "--label", "k"])).await["api_key"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(d.ping("A", &[("fuel", "1")], Some(&key)).await.unwrap().r#type, 1);
    // the probes stay open, the data endpoints do not
    assert_eq!(d.http_get("/healthz", None).await.0, 200);
    assert_eq!(d.http_get("/info", None).await.0, 401);
    assert_eq!(d.http_get("/metrics", None).await.0, 401);
    assert_eq!(d.http_get("/info", Some(&d.token)).await.0, 200);

    // exposing an open service is refused at startup
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("config")).unwrap();
    std::fs::write(tmp.path().join("config/config.json"), r#"{"bind":"all","security":{"mode":"open"}}"#).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_open-cardinal"))
        .current_dir(tmp.path())
        .env("CARDINAL_RUNTIME", "daemon")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("non-loopback"), "{}", String::from_utf8_lossy(&out.stderr));
}

// ---------------------------------------------------------------------------------------
// multi-tenancy, WASM, audit
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn tenants_are_isolated_in_rules_state_and_credentials() {
    let d = Daemon::start(
        Spec::single()
            .file(
                "tenants/acme/rules/default/r.lua",
                "local n = redb_api.incr('count') return { action = 'CUSTOM', cmd_name = 'ACME:' .. n }",
            )
            .file(
                "tenants/globex/rules/default/r.lua",
                "local n = redb_api.incr('count') return { action = 'CUSTOM', cmd_name = 'GLOBEX:' .. n }",
            )
            .file(
                "config/tenants.json",
                r#"{"tenants":[
                {"id":"acme","rules":{"lua":true}},
                {"id":"globex","rules":{"lua":true}}]}"#,
            ),
    )
    .await;
    let acme =
        d.data(Daemon::tenant("issue-key", &["acme", "--label", "k"])).await["api_key"].as_str().unwrap().to_string();
    let globex =
        d.data(Daemon::tenant("issue-key", &["globex", "--label", "k"])).await["api_key"].as_str().unwrap().to_string();

    assert_eq!(d.ping("a", &[], Some(&acme)).await.unwrap().command_name, "ACME:1");
    assert_eq!(d.ping("a", &[], Some(&acme)).await.unwrap().command_name, "ACME:2");
    // same key name 'count', different tenant: its own counter, its own rules
    assert_eq!(d.ping("a", &[], Some(&globex)).await.unwrap().command_name, "GLOBEX:1");
    assert_eq!(d.ping("a", &[], Some(&acme)).await.unwrap().command_name, "ACME:3");

    // overrides are per tenant too
    d.ok(Daemon::heathcliff("force", &["--tenant", "acme", "--agent", "a", "--force", "1"])).await;
    assert_eq!(d.ping("a", &[], Some(&acme)).await.unwrap().r#type, 1);
    assert_eq!(d.ping("a", &[], Some(&globex)).await.unwrap().r#type, 3, "globex's agent 'a' is unaffected");

    // a revoked key stops working at once
    d.ok(Daemon::tenant("revoke-key", &["acme", "k"])).await;
    assert_eq!(d.ping("a", &[], Some(&acme)).await.unwrap_err().code(), tonic::Code::Unauthenticated);
}

#[tokio::test(flavor = "multi_thread")]
async fn lua_is_off_by_default_for_new_tenants_but_wasm_is_on() {
    let json = r#"{"action":"RESTART","cmd_name":"FROM_WASM","priority":3}"#;
    let wasm = wat::parse_str(format!(
        r#"(module (memory (export "memory") 1)
          (data (i32.const 1024) "{}")
          (func (export "cardinal_alloc") (param i32) (result i32) (i32.const 8192))
          (func (export "cardinal_evaluate") (param i32 i32) (result i64)
            (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const {}))))"#,
        json.replace('"', "\\\""),
        json.len()
    ))
    .unwrap();
    let d = Daemon::start(
        Spec::single()
            .file("tenants/t1/rules/default/legacy.lua", "return { action = 'SHUTDOWN', priority = 999 }")
            .file("tenants/t1/rules/default/new.wasm", wasm)
            .file("config/tenants.json", r#"{"tenants":[{"id":"t1"}]}"#),
    )
    .await;
    let key =
        d.data(Daemon::tenant("issue-key", &["t1", "--label", "k"])).await["api_key"].as_str().unwrap().to_string();
    let r = d.ping("x", &[], Some(&key)).await.unwrap();
    assert_eq!(r.command_name, "FROM_WASM", "the WASM rule runs; the tenant's Lua rule was refused");
    let issues = d.data(CliRequest::Rules { command: "issues".into(), args: vec![] }).await;
    assert!(issues.to_string().contains("Lua rules are not enabled for this tenant"), "{issues}");
}

#[tokio::test(flavor = "multi_thread")]
async fn per_tenant_rate_limit() {
    let d = Daemon::start(Spec::single().file("rules/default/r.lua", "return nil").file(
        "config/tenants.json",
        r#"{"tenants":[{"id":"default","limits":{"rate_limit_per_sec":5,"rate_limit_burst":5}}]}"#,
    ))
    .await;
    let key = d.data(Daemon::tenant("issue-key", &["default", "--label", "k"])).await["api_key"]
        .as_str()
        .unwrap()
        .to_string();
    let mut c = d.grpc_client().await;
    let (mut ok, mut limited) = (0, 0);
    for _ in 0..40 {
        match ping_with(&mut c, "a", &[], Some(&key)).await {
            Ok(_) => ok += 1,
            Err(e) if e.code() == tonic::Code::ResourceExhausted => limited += 1,
            Err(e) => panic!("{e}"),
        }
    }
    assert!((5..=10).contains(&ok), "burst of 5 plus a little refill, got {ok}");
    assert!(limited >= 30);
}

#[tokio::test(flavor = "multi_thread")]
async fn audit_api_query_verify_export_and_tenant_scoping() {
    let d = Daemon::start(
        Spec::single()
            .file("tenants/acme/rules/default/r.lua", "return { action = 'RESTART', cmd_name = 'A' }")
            .file("tenants/globex/rules/default/r.lua", "return { action = 'RESTART', cmd_name = 'G' }")
            .file(
                "config/tenants.json",
                r#"{"tenants":[{"id":"acme","rules":{"lua":true}},{"id":"globex","rules":{"lua":true}}]}"#,
            ),
    )
    .await;
    let acme =
        d.data(Daemon::tenant("issue-key", &["acme", "--label", "k"])).await["api_key"].as_str().unwrap().to_string();
    let globex =
        d.data(Daemon::tenant("issue-key", &["globex", "--label", "k"])).await["api_key"].as_str().unwrap().to_string();
    for i in 0..5 {
        d.ping(&format!("a{i}"), &[("fuel", "50")], Some(&acme)).await.unwrap();
        d.ping(&format!("g{i}"), &[("fuel", "50")], Some(&globex)).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await; // writer batch

    // no credentials → 401; admin sees everything
    assert_eq!(d.http_get("/v1/audit", None).await.0, 401);
    let (code, _, body) = d.http_get("/v1/audit?event=decision&limit=100", Some(&d.token)).await;
    assert_eq!(code, 200);
    let all: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(all["records"].as_array().unwrap().len(), 10);
    let rec = &all["records"][0];
    assert_eq!(rec["event"], "decision");
    assert!(rec["data"]["rules"][0]["rule"].is_string(), "the record explains which rule decided: {rec}");
    assert!(rec["hash"].as_str().unwrap().len() == 64 && rec["prev"].as_str().unwrap().len() == 64);

    // a tenant key sees only its own tenant, and cannot ask for another one
    let (code, _, body) = d.http_get("/v1/audit?event=decision&limit=100", Some(&acme)).await;
    assert_eq!(code, 200);
    let mine: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(mine["records"].as_array().unwrap().len(), 5);
    assert!(mine["records"].as_array().unwrap().iter().all(|r| r["tenant"] == "acme"));
    assert_eq!(d.http_get("/v1/audit?tenant=globex", Some(&acme)).await.0, 403);
    assert_eq!(d.http_get("/v1/audit/verify", Some(&acme)).await.0, 403, "verify is admin-only");

    // chain verification and NDJSON export
    let (code, _, body) = d.http_get("/v1/audit/verify", Some(&d.token)).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["ok"], true);
    let (code, headers, body) = d.http_get("/v1/audit/export?from=1&limit=3", Some(&d.token)).await;
    assert_eq!(code, 200);
    assert_eq!(body.lines().count(), 3);
    assert!(headers.contains_key("x-next-seq"));
    for line in body.lines() {
        let _: Value = serde_json::from_str(line).expect("every export line is a JSON record");
    }

    // pagination
    let (_, _, p1) = d.http_get("/v1/audit?event=decision&limit=4", Some(&d.token)).await;
    let p1: Value = serde_json::from_str(&p1).unwrap();
    let next = p1["next_before"].as_u64().unwrap();
    let (_, _, p2) = d.http_get(&format!("/v1/audit?event=decision&limit=4&before={next}"), Some(&d.token)).await;
    let p2: Value = serde_json::from_str(&p2).unwrap();
    assert!(p2["records"][0]["seq"].as_u64().unwrap() < next);

    // CLI view of the same chain
    let v = d.data(CliRequest::Audit { command: "verify".into(), args: vec![] }).await;
    assert_eq!(v["ok"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn prometheus_metrics_and_health_probes() {
    let d = Daemon::start(legacy_spec()).await;
    d.ping("a", &[("fuel", "10")], None).await.unwrap();
    d.ping("a", &[("fuel", "100")], None).await.unwrap();
    assert_eq!(d.http_get("/healthz", None).await.2, "ok");
    let (code, _, body) = d.http_get("/readyz", None).await;
    assert_eq!(code, 200, "{body}");
    let (code, headers, text) = d.http_get("/metrics?format=prometheus", None).await;
    assert_eq!(code, 200);
    assert!(headers["content-type"].starts_with("text/plain; version=0.0.4"));
    assert!(text.contains("cardinal_pulses_total 2"), "{text}");
    assert!(text.contains("cardinal_reactions_total{action=\"shutdown\"} 1"));
    assert!(text.contains("cardinal_decision_latency_us_bucket"));
    // a real Prometheus scrape negotiates through the Accept header
    let mut raw = tokio::net::TcpStream::connect(std::net::SocketAddr::new(d.ip.into(), d.http)).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    raw.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nAccept: application/openmetrics-text;version=1.0.0;q=0.5,text/plain;version=0.0.4;q=0.3,*/*;q=0.1\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut buf = String::new();
    raw.read_to_string(&mut buf).await.unwrap();
    assert!(buf.contains("cardinal_pulses_total"));
}

#[tokio::test(flavor = "multi_thread")]
async fn graceful_stop_via_cli_answers_then_exits() {
    // original: `process::exit` in the middle of the request (Windows never even replied)
    let mut d = Daemon::start(legacy_spec()).await;
    let msg = d.ok(CliRequest::Stop).await;
    assert!(msg.contains("encerrando"), "{msg}");
    let mut exited = false;
    for _ in 0..150 {
        if !d.is_running() {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(exited, "daemon must exit after Stop");
    assert!(d.log().contains("Cardinal stopped"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_original_three_key_config_file_is_honoured_not_overwritten() {
    // original: config.json was overwritten with defaults on every start
    let spec = Spec::single();
    let (grpc, http, control) = (spec.grpc, spec.http, spec.control_port);
    let d = Daemon::start(Spec {
        config: json!({ "grpc_port": grpc, "http_port": http, "db_file": "custom.redb" }),
        ..spec
    })
    .await;
    assert!(d.path("custom.redb").exists(), "db_file from config.json is used");
    let text = std::fs::read_to_string(d.path("config/config.json")).unwrap();
    assert!(text.contains("custom.redb"));
    let _ = control;
}

#[tokio::test(flavor = "multi_thread")]
async fn audit_records_actions_by_default_and_everything_on_request() {
    // default: routine IDLE decisions are not recorded (1000 agents at 1 Hz would otherwise write
    // ~1 KiB each, 1M records in 17 minutes); anything that acted or failed is
    let d = Daemon::start(legacy_spec()).await;
    for _ in 0..20 {
        d.ping("Idle", &[("fuel", "100")], None).await.unwrap();
    }
    d.ping("Acts", &[("fuel", "10")], None).await.unwrap();
    let tail = d
        .data(CliRequest::Audit {
            command: "tail".into(),
            args: vec!["--event".into(), "decision".into(), "--limit".into(), "100".into()],
        })
        .await;
    let records = tail["records"].as_array().unwrap();
    assert_eq!(records.len(), 1, "{tail}");
    assert_eq!(records[0]["agent"], "Acts");

    let all = Daemon::start(legacy_spec().config(json!({ "audit": { "decisions": "all" } }))).await;
    for _ in 0..20 {
        all.ping("Idle", &[("fuel", "100")], None).await.unwrap();
    }
    let tail = all
        .data(CliRequest::Audit {
            command: "tail".into(),
            args: vec!["--event".into(), "decision".into(), "--limit".into(), "100".into()],
        })
        .await;
    assert_eq!(tail["records"].as_array().unwrap().len(), 20);
}

/// F-01 follow-up: the admin token file must not be readable by other local users. Unix gets
/// 0600; Windows has no mode bits, so the ACL is reduced to the current user.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn admin_token_file_is_restricted_to_the_current_user_on_windows() {
    let d = Daemon::start(legacy_spec()).await;
    let out = std::process::Command::new("icacls").arg(d.path("config/admin.token")).output().unwrap();
    let acl = String::from_utf8_lossy(&out.stdout).to_string();
    let user = std::env::var("USERNAME").unwrap();
    assert!(acl.contains(&user), "{acl}");
    for other in ["Everyone", "BUILTIN\\Users", "Authenticated Users"] {
        assert!(!acl.contains(other), "{other} must not have access: {acl}");
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn admin_token_file_is_owner_only_on_unix() {
    use std::os::unix::fs::PermissionsExt;
    let d = Daemon::start(legacy_spec()).await;
    let mode = std::fs::metadata(d.path("config/admin.token")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}
