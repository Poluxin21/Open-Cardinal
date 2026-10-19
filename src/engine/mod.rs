//! The decision engine: evaluates every rule of an agent and runs the priority auction.
//!
//! Pulse flow: look up the compiled rules (no disk access) → take a per-tenant and a
//! global concurrency permit → run the rules on a blocking thread under instruction,
//! memory and wall-clock budgets → highest priority wins (first rule wins ties, rules
//! run in file-name order, so the result is deterministic).

pub mod backends;
pub mod mem;
pub mod registry;
pub mod types;

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

pub use registry::{AiRuleFactory, LoadIssue, Lookup, Rule, RuleRegistry, RuleSet};
pub use types::{
    EvalCtx, Host, PulseInput, RuleBackend, RuleError, RuleErrorKind, RuleKind, RuleOutput, idle_reaction,
};

use crate::config::Limits;
use crate::error::{Error, Result};
use crate::pb::core::Reaction;
use crate::tenant::Tenant;
use mem::SharedMem;

/// A decision at or above this priority is final: remaining rules (including slow AI
/// rules) are skipped. Same threshold as the original engine.
pub const EMERGENCY_PRIORITY: i32 = 1000;

#[derive(Debug, Clone)]
pub enum OutcomeKind {
    NoOpinion,
    Output {
        action: String,
        priority: i32,
        evidence: Option<String>,
    },
    Error {
        kind: RuleErrorKind,
        message: String,
    },
    /// Not evaluated (an emergency decision was already taken, or the budget ran out).
    Skipped,
}

#[derive(Debug, Clone)]
pub struct RuleOutcome {
    pub rule: String,
    pub kind: RuleKind,
    pub micros: u64,
    pub result: OutcomeKind,
}

#[derive(Debug)]
pub struct Decision {
    pub reaction: Reaction,
    /// Rule that produced the reaction (`None` for idle fallbacks).
    pub winner: Option<String>,
    pub winner_kind: Option<RuleKind>,
    pub priority: i32,
    pub outcomes: Vec<RuleOutcome>,
    pub elapsed: Duration,
}

impl Decision {
    pub fn rule_errors(&self) -> u64 {
        self.outcomes.iter().filter(|o| matches!(o.result, OutcomeKind::Error { .. })).count() as u64
    }

    fn idle(trace_id: &str, command_name: &str, started: Instant) -> Self {
        Self {
            reaction: idle_reaction(trace_id, command_name),
            winner: None,
            winner_kind: None,
            priority: 0,
            outcomes: Vec::new(),
            elapsed: started.elapsed(),
        }
    }
}

pub struct Engine {
    registry: Arc<RuleRegistry>,
    host: Arc<dyn Host>,
    global_gate: Arc<Semaphore>,
}

impl Engine {
    pub fn new(registry: Arc<RuleRegistry>, host: Arc<dyn Host>, global_limits: &Limits) -> Self {
        Self { registry, host, global_gate: Arc::new(Semaphore::new(global_limits.concurrency())) }
    }

    pub fn registry(&self) -> &Arc<RuleRegistry> {
        &self.registry
    }

    pub async fn evaluate(&self, tenant: &Arc<Tenant>, input: PulseInput) -> Result<Decision> {
        let started = Instant::now();
        let trace_id = input.trace_id.clone();

        let set = self.registry.current();
        let rules: Vec<Arc<Rule>> = match set.lookup(&tenant.id, &input.agent_id) {
            Lookup::Agent(r) | Lookup::Default(r) if !r.is_empty() => r.to_vec(),
            // the rules directory exists but holds nothing runnable
            Lookup::Agent(_) | Lookup::Default(_) => return Ok(Decision::idle(&trace_id, "NO_ACTION", started)),
            Lookup::None => return Ok(Decision::idle(&trace_id, "DIR_NOT_FOUND", started)),
        };
        drop(set);

        let limits = tenant.limits.clone();
        let wait = Duration::from_millis(limits.rule_timeout_ms);
        let tenant_permit = tokio::time::timeout(wait, tenant.gate.clone().acquire_owned())
            .await
            .map_err(|_| Error::Overloaded(format!("tenant '{}' has too many evaluations in flight", tenant.id)))?
            .map_err(|_| Error::Other("tenant gate closed".into()))?;
        let global_permit = tokio::time::timeout(wait, self.global_gate.clone().acquire_owned())
            .await
            .map_err(|_| Error::Overloaded("server is saturated".into()))?
            .map_err(|_| Error::Other("gate closed".into()))?;

        let has_ai = tenant.ai.enabled && rules.iter().any(|r| r.kind.is_ai());
        let hard_stop = wait
            + if has_ai { Duration::from_millis(limits.ai_timeout_ms) } else { Duration::ZERO }
            + Duration::from_millis(500);

        let host = self.host.clone();
        // The permits travel with the blocking task: if a rule gets stuck inside native
        // code the slot stays occupied, so a stuck tenant exhausts *its own* gate instead
        // of the whole daemon.
        let task = tokio::task::spawn_blocking(move || {
            let _permits = (tenant_permit, global_permit);
            run_rules(&rules, &input, host, &limits)
        });

        match tokio::time::timeout(hard_stop, task).await {
            Ok(Ok(run)) => {
                let (outcomes, best) = run;
                let reaction_for = |out: RuleOutput| out.into_reaction(&trace_id);
                Ok(match best {
                    Some(best) => Decision {
                        priority: best.priority,
                        winner: Some(best.rule),
                        winner_kind: Some(best.kind),
                        reaction: reaction_for(best.output),
                        outcomes,
                        elapsed: started.elapsed(),
                    },
                    None => Decision { outcomes, ..Decision::idle(&trace_id, "NO_ACTION", started) },
                })
            }
            Ok(Err(join)) => {
                tracing::error!(tenant = %tenant.id, "rule task failed: {join}");
                let mut d = Decision::idle(&trace_id, "RULE_FAILURE", started);
                d.outcomes.push(RuleOutcome {
                    rule: "<engine>".into(),
                    kind: RuleKind::Lua,
                    micros: started.elapsed().as_micros() as u64,
                    result: OutcomeKind::Error { kind: RuleErrorKind::Runtime, message: "rule task panicked".into() },
                });
                Ok(d)
            }
            Err(_) => {
                tracing::error!(tenant = %tenant.id, "rules exceeded the hard stop of {hard_stop:?}; failing safe");
                let mut d = Decision::idle(&trace_id, "RULE_TIMEOUT", started);
                d.outcomes.push(RuleOutcome {
                    rule: "<engine>".into(),
                    kind: RuleKind::Lua,
                    micros: started.elapsed().as_micros() as u64,
                    result: OutcomeKind::Error {
                        kind: RuleErrorKind::Timeout,
                        message: "evaluation did not return".into(),
                    },
                });
                Ok(d)
            }
        }
    }
}

struct Best {
    rule: String,
    kind: RuleKind,
    priority: i32,
    output: RuleOutput,
}

fn run_rules(
    rules: &[Arc<Rule>],
    input: &PulseInput,
    host: Arc<dyn Host>,
    limits: &Limits,
) -> (Vec<RuleOutcome>, Option<Best>) {
    let mem = Arc::new(SharedMem::new(host, &input.tenant, limits.kv_max_entries, limits.kv_max_writes_per_eval));
    let start = Instant::now();
    let det_deadline = start + Duration::from_millis(limits.rule_timeout_ms);

    let mut outcomes = Vec::with_capacity(rules.len());
    // `-1` start value: any rule with priority >= 0 can win, negative ones never do
    let mut best: Option<Best> = None;
    let mut best_priority = -1;
    let mut stop = false;

    for rule in rules {
        let skip = |outcomes: &mut Vec<RuleOutcome>| {
            outcomes.push(RuleOutcome {
                rule: rule.name.clone(),
                kind: rule.kind,
                micros: 0,
                result: OutcomeKind::Skipped,
            })
        };
        if stop {
            skip(&mut outcomes);
            continue;
        }

        let now = Instant::now();
        let deadline = if rule.kind.is_ai() {
            now + Duration::from_millis(rule.timeout_ms.map_or(limits.ai_timeout_ms, |t| t.min(limits.ai_timeout_ms)))
        } else {
            let own = now + Duration::from_millis(rule.timeout_ms.unwrap_or(limits.rule_timeout_ms));
            own.min(det_deadline)
        };
        if now >= deadline {
            outcomes.push(RuleOutcome {
                rule: rule.name.clone(),
                kind: rule.kind,
                micros: 0,
                result: OutcomeKind::Error {
                    kind: RuleErrorKind::Timeout,
                    message: "evaluation budget already spent".into(),
                },
            });
            continue;
        }

        mem.set_deadline(deadline);
        let cx = EvalCtx { input, mem: mem.clone(), limits, deadline };
        let t0 = Instant::now();
        let evaluated = std::panic::catch_unwind(AssertUnwindSafe(|| rule.backend.evaluate(&cx)))
            .unwrap_or_else(|_| Err(RuleError::runtime("rule backend panicked")));
        let micros = t0.elapsed().as_micros() as u64;

        let result = match evaluated {
            Ok(None) => OutcomeKind::NoOpinion,
            Ok(Some(mut out)) => match out.action_type() {
                None => OutcomeKind::Error {
                    kind: RuleErrorKind::Contract,
                    message: format!("unknown action '{}'", crate::util::log_safe(&out.action)),
                },
                Some(_) => {
                    let mut priority = out.priority();
                    if let Some(cap) = rule.priority_cap {
                        priority = priority.min(cap);
                        out.priority = Some(priority);
                    }
                    if priority > best_priority {
                        best_priority = priority;
                        best = Some(Best { rule: rule.name.clone(), kind: rule.kind, priority, output: out.clone() });
                        if priority >= EMERGENCY_PRIORITY {
                            stop = true;
                        }
                    }
                    OutcomeKind::Output {
                        action: out.action.to_ascii_uppercase(),
                        priority,
                        evidence: out.evidence.clone(),
                    }
                }
            },
            Err(e) => {
                tracing::warn!(
                    tenant = %input.tenant,
                    agent = %crate::util::log_safe(&input.agent_id),
                    rule = %rule.name,
                    "rule failed: {}",
                    crate::util::log_safe(&e.to_string())
                );
                OutcomeKind::Error { kind: e.kind, message: e.message }
            }
        };
        outcomes.push(RuleOutcome { rule: rule.name.clone(), kind: rule.kind, micros, result });
    }
    (outcomes, best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::store::Store;
    use crate::tenant::TenantRegistry;
    use mem::testing::DirectHost;

    pub(crate) struct Fixture {
        pub _dir: tempfile::TempDir,
        pub paths: Paths,
        pub tenants: TenantRegistry,
        pub store: Arc<Store>,
        pub engine: Engine,
    }

    pub(crate) fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let tenants = TenantRegistry::load(paths.clone(), Limits::default(), crate::config::AuthMode::Open).unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let registry = Arc::new(RuleRegistry::new(None));
        let engine = Engine::new(registry, Arc::new(DirectHost(store.clone())), &Limits::default());
        Fixture { _dir: dir, paths, tenants, store, engine }
    }

    impl Fixture {
        pub fn rule(&self, rel: &str, text: &str) {
            let p = self.paths.rules_dir().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        pub fn reload(&self) {
            self.engine.registry().reload(&self.tenants.list());
        }
        pub async fn pulse(&self, agent: &str, kv: &[(&str, &str)]) -> Decision {
            let tenant = self.tenants.get("default").unwrap();
            let input = PulseInput {
                agent_id: agent.into(),
                tenant: "default".into(),
                timestamp: 0,
                trace_id: "trace".into(),
                telemetry: kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            };
            self.engine.evaluate(&tenant, input).await.unwrap()
        }
    }

    const SHUTDOWN_LOW_FUEL: &str = r#"
local f = tonumber(pulse.telemetry["fuel"]) or 0
if f < 70 then return { action = "SHUTDOWN", cmd_name = "EMERGENCY_CUTOFF", priority = 1000, params = { reason = "Overheating" } } end
"#;

    #[tokio::test]
    async fn default_rule_decides_like_the_original() {
        let fx = fixture();
        fx.rule("default/default.lua", SHUTDOWN_LOW_FUEL);
        fx.reload();
        let d = fx.pulse("Rocket_01", &[("fuel", "53")]).await;
        assert_eq!(d.reaction.r#type, 1);
        assert_eq!(d.reaction.command_name, "EMERGENCY_CUTOFF");
        assert_eq!(d.reaction.parameters["reason"], "Overheating");
        assert_eq!(d.winner.as_deref(), Some("default"));
        let d = fx.pulse("Rocket_01", &[("fuel", "89")]).await;
        assert_eq!(d.reaction.r#type, 0);
        assert_eq!(d.reaction.command_name, "NO_ACTION");
    }

    #[tokio::test]
    async fn missing_rules_directory_is_dir_not_found() {
        let fx = fixture();
        fx.reload();
        assert_eq!(fx.pulse("x", &[]).await.reaction.command_name, "DIR_NOT_FOUND");
    }

    #[tokio::test]
    async fn highest_priority_wins_and_ties_go_to_the_first_file() {
        let fx = fixture();
        fx.rule("A/a_low.lua", "return { action = 'RESTART', priority = 10 }");
        fx.rule("A/b_high.lua", "return { action = 'SHUTDOWN', priority = 500 }");
        fx.rule("A/c_tie.lua", "return { action = 'CUSTOM', priority = 500 }");
        fx.reload();
        let d = fx.pulse("A", &[]).await;
        assert_eq!(d.reaction.r#type, 1);
        assert_eq!(d.winner.as_deref(), Some("b_high"));
    }

    #[tokio::test]
    async fn emergency_priority_skips_the_remaining_rules() {
        let fx = fixture();
        fx.rule("A/a.lua", "return { action = 'SHUTDOWN', priority = 1000 }");
        fx.rule("A/b.lua", "return { action = 'RESTART', priority = 5 }");
        fx.reload();
        let d = fx.pulse("A", &[]).await;
        assert!(matches!(d.outcomes[1].result, OutcomeKind::Skipped));
    }

    #[tokio::test]
    async fn a_failing_rule_does_not_block_the_others() {
        let fx = fixture();
        fx.rule("A/a_broken.lua", "error('boom')");
        fx.rule("A/b_loop.lua", "while true do end");
        fx.rule("A/c_typo.lua", "return { action = 'SHUTDWN', priority = 900 }");
        fx.rule("A/d_ok.lua", "return { action = 'RESTART', priority = 1 }");
        fx.reload();
        let d = fx.pulse("A", &[]).await;
        assert_eq!(d.reaction.r#type, 2, "the valid low-priority rule still decides");
        assert_eq!(d.rule_errors(), 3);
        // the typo'd action must not win with priority 900 (the original silently mapped it to IDLE)
        assert_eq!(d.winner.as_deref(), Some("d_ok"));
    }

    #[tokio::test]
    async fn infinite_loop_rule_cannot_freeze_the_engine() {
        // PoC 4: 22 pulses on a looping rule used to hang every thread of the daemon
        let fx = Arc::new(fixture());
        fx.rule("Loop/loop.lua", "while true do end");
        fx.reload();
        let started = Instant::now();
        let tenant = fx.tenants.get("default").unwrap();
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..64 {
            let fx = fx.clone();
            let tenant = tenant.clone();
            set.spawn(async move {
                let input = PulseInput {
                    agent_id: "Loop".into(),
                    tenant: "default".into(),
                    timestamp: 0,
                    trace_id: "t".into(),
                    telemetry: Default::default(),
                };
                fx.engine.evaluate(&tenant, input).await
            });
        }
        let (mut decided, mut refused) = (0, 0);
        while let Some(r) = set.join_next().await {
            match r.unwrap() {
                Ok(d) => {
                    assert_eq!(d.rule_errors(), 1, "the loop is stopped by its budget");
                    decided += 1;
                }
                // under saturation the engine refuses fast instead of queueing (by design)
                Err(Error::Overloaded(_)) => refused += 1,
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(decided + refused, 64);
        assert!(decided > 0, "some pulses must get through");
        // meanwhile ordinary pulses were never starved
        fx.rule("Ok/ok.lua", "return { action = 'RESTART' }");
        fx.reload();
        assert_eq!(fx.pulse("Ok", &[]).await.reaction.r#type, 2);
        assert!(started.elapsed() < Duration::from_secs(20), "took {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn persistence_survives_between_pulses() {
        let fx = fixture();
        fx.rule("V/strikes.lua", "return { action = 'CUSTOM', cmd_name = tostring(redb_api.incr('n')) }");
        fx.reload();
        assert_eq!(fx.pulse("V", &[]).await.reaction.command_name, "1");
        assert_eq!(fx.pulse("V", &[]).await.reaction.command_name, "2");
        assert_eq!(fx.store.kv_get("default", "n").unwrap(), Some(2));
    }

    #[tokio::test]
    async fn reload_swaps_rules_atomically() {
        let fx = fixture();
        fx.rule("A/x.lua", "return { action = 'RESTART' }");
        fx.reload();
        assert_eq!(fx.pulse("A", &[]).await.reaction.r#type, 2);
        fx.rule("A/x.lua", "return { action = 'SHUTDOWN' }");
        assert_eq!(fx.pulse("A", &[]).await.reaction.r#type, 2, "still the old compiled rule until reload");
        fx.reload();
        assert_eq!(fx.pulse("A", &[]).await.reaction.r#type, 1);
    }

    #[tokio::test]
    async fn priority_cap_limits_a_rule() {
        let fx = fixture();
        fx.rule("A/a.lua", "return { action = 'SHUTDOWN', priority = 1000 }");
        fx.rule("A/a.rule.json", r#"{"priority_cap": 50}"#);
        fx.rule("A/b.lua", "return { action = 'RESTART', priority = 100 }");
        fx.reload();
        let d = fx.pulse("A", &[]).await;
        assert_eq!(d.reaction.r#type, 2, "capped rule (50) loses to b (100) and no longer short-circuits");
    }
}
