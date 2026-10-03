//! The files under `examples/` are documentation: they must keep parsing with the real
//! parsers and the example rules must keep working.

mod common;

use std::path::Path;

use common::*;
use open_cardinal::config::Config;
use open_cardinal::raft::config::RaftFile;
use open_cardinal::tenant::TenantsFile;

fn example(rel: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(rel)).unwrap()
}

#[test]
fn example_configuration_files_parse_with_the_real_parsers() {
    let cfg: Config = serde_json::from_str(&example("config/config.json")).expect("config.json");
    assert_eq!(cfg.audit.retention_records, 500_000);

    let tenants: TenantsFile = serde_json::from_str(&example("config/tenants.json")).expect("tenants.json");
    tenants.validate().expect("tenants.json validates");
    assert_eq!(tenants.tenants[0].id, "acme");

    let raft: RaftFile = serde_json::from_str(&example("config/raft.json")).expect("raft.json");
    assert_eq!(raft.peers.len(), 3);

    #[derive(serde::Deserialize)]
    struct Models {
        models: Vec<serde_json::Value>,
    }
    let models: Models = serde_json::from_str(&example("config/models.json")).expect("models.json");
    assert_eq!(models.models.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn example_lua_and_wasm_rules_work_end_to_end() {
    let wasm = wat::parse_str(example("rules/wasm/low_fuel.wat")).expect("the WAT example assembles");
    let d = Daemon::start(
        Spec::single()
            .file("rules/default/default.lua", example("rules/lua/default.lua"))
            .file("rules/Wasm/low_fuel.wasm", wasm)
            .file("rules/Strikes/strikes.lua", example("rules/lua/strikes.lua")),
    )
    .await;

    // the WASM twin of the default rule
    let r = d.ping("Wasm", &[("fuel", "53")], None).await.unwrap();
    assert_eq!((r.r#type, r.command_name.as_str()), (1, "EMERGENCY_CUTOFF"));
    assert_eq!(d.ping("Wasm", &[("fuel", "89")], None).await.unwrap().r#type, 0);

    // the Lua strikes example (uses incr / get-returns-nil)
    for (i, expect_shutdown) in [(1, false), (2, false), (3, true)] {
        let r = d.ping("Strikes", &[("cpu_temp", "99")], None).await.unwrap();
        assert_eq!(r.r#type == 1, expect_shutdown, "reading {i}");
    }
    let issues =
        d.data(open_cardinal::control::protocol::CliRequest::Rules { command: "issues".into(), args: vec![] }).await;
    assert!(issues["issues"].as_array().unwrap().is_empty(), "{issues}");
}
