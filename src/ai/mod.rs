//! AI rules: models running inside the daemon through an embedded ONNX Runtime.
//! Compiled in with `--features onnx` (classifier/regression models) and
//! `--features prompt` (a causal language model that evaluates a natural-language rule).
//!
//! Design principles, because this sits in the decision path of critical systems:
//!
//! * **Models are registered by the operator** in `config/models.json` (path confined to
//!   `models/`, optional SHA-256 pin). Tenants can only *reference* a model by name, and only
//!   the ones their policy allows. A tenant can never make the daemon load an arbitrary file.
//! * **The AI chooses from a menu a human wrote.** A model rule maps scores to reactions
//!   declared in its manifest; a prompt rule makes the model pick one label out of the
//!   declared choices. The model never invents actions, commands or parameters.
//! * **AI never outranks the deterministic rules.** Every AI decision is capped at the
//!   tenant's `ai.max_priority` (default 500), below the 1000 "emergency" level.
//! * **Everything is auditable.** Each decision carries evidence (model name, file hash,
//!   scores) into the audit trail, and failures/timeouts degrade to "no opinion".

#[cfg(feature = "onnx")]
mod classifier;
#[cfg(feature = "onnx")]
mod models;
#[cfg(feature = "prompt")]
mod prompt;
#[cfg(all(test, feature = "onnx"))]
mod testutil;

use std::sync::Arc;

use crate::config::Settings;
use crate::engine::AiRuleFactory;
use crate::error::Result;

/// Build the factory that turns `type: model|prompt` manifests into rules, when this
/// binary has AI support.
#[cfg(feature = "onnx")]
pub fn factory(settings: &Settings) -> Result<Option<Arc<dyn AiRuleFactory>>> {
    Ok(Some(Arc::new(models::OnnxFactory::new(&settings.paths))))
}

#[cfg(not(feature = "onnx"))]
pub fn factory(_settings: &Settings) -> Result<Option<Arc<dyn AiRuleFactory>>> {
    Ok(None)
}

/// Whether this build can run AI rules (for status output).
pub const fn available() -> bool {
    cfg!(feature = "onnx")
}

#[cfg(all(test, feature = "onnx"))]
mod e2e {
    use std::sync::Arc;

    use crate::config::{AuthMode, Limits, Paths};
    use crate::engine::mem::testing::DirectHost;
    use crate::engine::{Engine, PulseInput, RuleRegistry};
    use crate::store::Store;
    use crate::tenant::TenantRegistry;

    /// manifest → registry → engine → reaction, with the AI priority ceiling enforced.
    #[tokio::test]
    async fn ai_rule_runs_through_the_engine_and_never_outranks_a_deterministic_rule() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::create_dir_all(paths.models_dir()).unwrap();
        std::fs::create_dir_all(paths.rules_dir().join("default")).unwrap();
        // identity model: score = feature
        std::fs::write(paths.models_dir().join("m.onnx"), super::testutil::linear_model(&[1.0], &[0.0], 1, 1)).unwrap();
        std::fs::write(paths.models_file(), r#"{"models":[{"name":"m","path":"m.onnx"}]}"#).unwrap();
        let rules = paths.rules_dir().join("default");
        std::fs::write(
            rules.join("anomaly.rule.json"),
            r#"{"type":"model","model":"m","features":[{"key":"temp"}],
                "decide":{"mode":"threshold","bands":[{"ge":0.5,"action":"SHUTDOWN","cmd_name":"AI","priority":1000}]}}"#,
        )
        .unwrap();
        std::fs::write(
            rules.join("guard.lua"),
            "return { action = 'RESTART', cmd_name = 'DETERMINISTIC', priority = 600 }",
        )
        .unwrap();

        let tenants = TenantRegistry::load(paths.clone(), Limits::default(), AuthMode::Open).unwrap();
        tenants
            .edit(|f| {
                f.add_tenant("default")?;
                let t = &mut f.tenants[0];
                t.ai.enabled = true;
                t.ai.models = vec!["m".into()];
                t.ai.max_priority = 500;
                Ok(())
            })
            .unwrap();

        let store = Arc::new(Store::open_in_memory().unwrap());
        let registry = Arc::new(RuleRegistry::new(Some(Arc::new(super::models::OnnxFactory::new(&paths)))));
        let engine = Engine::new(registry.clone(), Arc::new(DirectHost(store)), &Limits::default());
        let set = registry.reload(&tenants.list());
        assert!(set.issues.is_empty(), "{:?}", set.issues);
        assert_eq!(set.total_rules(), 2);

        let tenant = tenants.get("default").unwrap();
        let pulse = |temp: &str| PulseInput {
            agent_id: "any".into(),
            tenant: "default".into(),
            timestamp: 0,
            trace_id: "t".into(),
            telemetry: [("temp".to_string(), temp.to_string())].into(),
        };
        let d = engine.evaluate(&tenant, pulse("0.9")).await.unwrap();
        // the model says SHUTDOWN at priority 1000, but AI is capped at 500: the Lua guard wins
        assert_eq!(d.reaction.command_name, "DETERMINISTIC");
        assert_eq!(d.winner.as_deref(), Some("guard"));
        // ...and the AI rule still ran (it did not short-circuit, it was capped)
        assert!(
            d.outcomes.iter().any(|o| o.rule == "anomaly"
                && matches!(&o.result, crate::engine::OutcomeKind::Output { priority: 500, evidence: Some(_), .. })),
            "{:?}",
            d.outcomes
        );

        // disable the deterministic guard: now the AI decides, with its evidence recorded
        std::fs::remove_file(paths.rules_dir().join("default/guard.lua")).unwrap();
        registry.reload(&tenants.list());
        let d = engine.evaluate(&tenant, pulse("0.9")).await.unwrap();
        assert_eq!(d.reaction.command_name, "AI");
        assert_eq!(d.priority, 500);
        assert_eq!(d.winner_kind.map(|k| k.as_str()), Some("model"));
    }

    #[tokio::test]
    async fn tenants_without_ai_policy_get_a_clear_load_issue() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::create_dir_all(paths.rules_dir().join("default")).unwrap();
        std::fs::write(paths.rules_dir().join("default/a.rule.json"), r#"{"type":"model","model":"m","features":[{"key":"t"}],"decide":{"mode":"threshold","bands":[{"ge":1,"action":"IDLE"}]}}"#).unwrap();
        let tenants = TenantRegistry::load(paths.clone(), Limits::default(), AuthMode::Open).unwrap();
        let registry = RuleRegistry::new(Some(Arc::new(super::models::OnnxFactory::new(&paths))));
        let set = registry.reload(&tenants.list());
        assert_eq!(set.total_rules(), 0);
        assert!(set.issues[0].message.contains("AI rules are disabled for this tenant"), "{:?}", set.issues);
    }
}
