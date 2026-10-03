//! Legacy Lua backend, now sandboxed.
//!
//! What changed compared to the original `Lua::new()`:
//! * only `table`, `string`, `math` and `utf8` are loaded — no `os`, `io`, `package`,
//!   `debug` or `coroutine`, and `dofile`/`loadfile`/`load`/`print`/`collectgarbage` are
//!   gone (PoC 3: `os.execute` in a rule file ran as the daemon user);
//! * every evaluation has an instruction budget, a heap limit and a wall-clock deadline
//!   enforced from a VM hook (PoC 4: `while true do end` froze the whole daemon);
//! * rules are only *compiled* when loaded, never executed without a `pulse`;
//! * `redb_api.get` returns `nil` for an absent key instead of panicking.
//!
//! Lua is still best treated as trusted-author code: a single C-side pattern match can
//! outrun the instruction hook. Hostile tenants should use WASM rules (fuel metered).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use mlua::{HookTriggers, Lua, LuaOptions, LuaSerdeExt, StdLib, Value, VmState};

use crate::engine::types::{EvalCtx, RuleBackend, RuleError, RuleErrorKind, RuleKind, RuleOutput};

/// VM instructions between two hook invocations (granularity of budget/deadline checks).
const HOOK_STRIDE: u32 = 1_000;

pub struct LuaRule {
    name: String,
    source: Arc<str>,
}

impl LuaRule {
    /// Validate the syntax without executing anything.
    pub fn new(name: &str, source: String) -> Result<Self, RuleError> {
        let lua = new_vm().map_err(RuleError::runtime)?;
        lua.load(&source)
            .set_name(name)
            .into_function()
            .map_err(|e| RuleError::runtime(format!("syntax error in {name}: {e}")))?;
        Ok(Self { name: name.to_string(), source: Arc::from(source) })
    }
}

fn new_vm() -> mlua::Result<Lua> {
    let lua = Lua::new_with(StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8, LuaOptions::new())?;
    let g = lua.globals();
    for name in [
        "dofile",
        "loadfile",
        "load",
        "loadstring",
        "require",
        "collectgarbage",
        "print",
        "os",
        "io",
        "package",
        "debug",
    ] {
        g.set(name, Value::Nil)?;
    }
    Ok(lua)
}

impl RuleBackend for LuaRule {
    fn kind(&self) -> RuleKind {
        RuleKind::Lua
    }

    fn evaluate(&self, cx: &EvalCtx<'_>) -> Result<Option<RuleOutput>, RuleError> {
        let lua = new_vm().map_err(RuleError::runtime)?;
        lua.set_memory_limit(cx.limits.lua_memory_bytes).map_err(RuleError::runtime)?;

        // deterministic randomness: same pulse => same numbers
        install_seed(&lua, cx).map_err(RuleError::runtime)?;
        install_limits(&lua, cx.deadline, cx.limits.lua_instructions).map_err(RuleError::runtime)?;
        install_api(&lua, cx).map_err(RuleError::runtime)?;

        let pulse = lua.to_value(cx.input).map_err(RuleError::runtime)?;
        lua.globals().set("pulse", pulse).map_err(RuleError::runtime)?;

        let value: Value = lua.load(&*self.source).set_name(&self.name).eval().map_err(|e| classify(&e))?;

        if matches!(value, Value::Nil) {
            return Ok(None);
        }
        lua.from_value::<RuleOutput>(value)
            .map(Some)
            .map_err(|e| RuleError::contract(format!("invalid return value: {e}")))
    }
}

fn install_seed(lua: &Lua, cx: &EvalCtx<'_>) -> mlua::Result<()> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    cx.input.agent_id.hash(&mut h);
    cx.input.timestamp.hash(&mut h);
    let seed = (h.finish() >> 1) as i64;
    lua.load("math.randomseed(...)").call::<()>(seed)
}

fn install_limits(lua: &Lua, deadline: Instant, budget: u64) -> mlua::Result<()> {
    let remaining = Rc::new(Cell::new(budget as i64));
    lua.set_hook(HookTriggers::new().every_nth_instruction(HOOK_STRIDE), move |_, _| {
        let left = remaining.get() - HOOK_STRIDE as i64;
        remaining.set(left);
        if left <= 0 {
            return Err(mlua::Error::runtime("cardinal:budget instruction budget exhausted"));
        }
        if Instant::now() >= deadline {
            return Err(mlua::Error::runtime("cardinal:timeout evaluation deadline exceeded"));
        }
        Ok(VmState::Continue)
    })
}

fn install_api(lua: &Lua, cx: &EvalCtx<'_>) -> mlua::Result<()> {
    let api = lua.create_table()?;

    let mem = cx.mem.clone();
    api.set("get", lua.create_function(move |_, key: String| mem.get(&key).map_err(mlua::Error::external))?)?;

    let mem = cx.mem.clone();
    api.set(
        "set",
        lua.create_function(move |_, (key, value): (String, Value)| {
            let v = to_u64(&value)?;
            mem.set(&key, v).map_err(mlua::Error::external)
        })?,
    )?;

    let mem = cx.mem.clone();
    api.set(
        "incr",
        lua.create_function(move |_, (key, delta): (String, Option<Value>)| {
            let d = match delta {
                None | Some(Value::Nil) => 1,
                Some(v) => to_i64(&v)?,
            };
            mem.add(&key, d).map_err(mlua::Error::external)
        })?,
    )?;

    let mem = cx.mem.clone();
    api.set("del", lua.create_function(move |_, key: String| mem.delete(&key).map_err(mlua::Error::external))?)?;
    lua.globals().set("redb_api", api)?;

    // cardinal.log(msg): rule diagnostics land in the daemon log with tenant/agent context
    let cardinal = lua.create_table()?;
    let tenant = cx.input.tenant.clone();
    let agent = cx.input.agent_id.clone();
    cardinal.set(
        "log",
        lua.create_function(move |_, msg: String| {
            tracing::info!(tenant = %tenant, agent = %crate::util::log_safe(&agent), "rule: {}", crate::util::log_safe(&msg));
            Ok(())
        })?,
    )?;
    lua.globals().set("cardinal", cardinal)?;
    Ok(())
}

fn to_u64(v: &Value) -> mlua::Result<u64> {
    match v {
        Value::Integer(i) if *i >= 0 => Ok(*i as u64),
        Value::Number(n) if n.fract() == 0.0 && *n >= 0.0 && *n < 18_446_744_073_709_551_615.0 => Ok(*n as u64),
        _ => Err(mlua::Error::runtime("value must be a non-negative integer")),
    }
}

fn to_i64(v: &Value) -> mlua::Result<i64> {
    match v {
        Value::Integer(i) => Ok(*i),
        Value::Number(n) if n.fract() == 0.0 && n.abs() < 9.2e18 => Ok(*n as i64),
        _ => Err(mlua::Error::runtime("delta must be an integer")),
    }
}

/// Map a Lua error to our taxonomy, looking through callback wrappers.
fn classify(e: &mlua::Error) -> RuleError {
    if let Some(re) = e.downcast_ref::<RuleError>() {
        return re.clone();
    }
    if let mlua::Error::CallbackError { cause, .. } = e {
        return classify(cause);
    }
    if matches!(e, mlua::Error::MemoryError(_)) {
        return RuleError::new(RuleErrorKind::Budget, "lua heap limit exceeded");
    }
    let text = e.to_string();
    if text.contains("cardinal:timeout") {
        return RuleError::new(RuleErrorKind::Timeout, "evaluation deadline exceeded");
    }
    if text.contains("cardinal:budget") {
        return RuleError::new(RuleErrorKind::Budget, "instruction budget exhausted");
    }
    if text.contains("not enough memory") {
        return RuleError::new(RuleErrorKind::Budget, "lua heap limit exceeded");
    }
    RuleError::runtime(text.lines().next().unwrap_or("lua error"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Limits;
    use crate::engine::mem::testing::mem;
    use crate::engine::types::PulseInput;
    use crate::store::Store;
    use std::collections::HashMap;
    use std::time::Duration;

    fn input(telemetry: &[(&str, &str)]) -> PulseInput {
        PulseInput {
            agent_id: "Rocket_01".into(),
            tenant: "default".into(),
            timestamp: 1_700_000_000,
            trace_id: "t".into(),
            telemetry: telemetry.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>(),
        }
    }

    fn run(src: &str, telemetry: &[(&str, &str)]) -> Result<Option<RuleOutput>, RuleError> {
        run_with(src, telemetry, Limits::default(), Duration::from_secs(5))
    }

    fn run_with(
        src: &str,
        telemetry: &[(&str, &str)],
        limits: Limits,
        budget: Duration,
    ) -> Result<Option<RuleOutput>, RuleError> {
        let store = Arc::new(Store::open_in_memory().unwrap());
        run_on(&store, src, telemetry, limits, budget)
    }

    fn run_on(
        store: &Arc<Store>,
        src: &str,
        telemetry: &[(&str, &str)],
        limits: Limits,
        budget: Duration,
    ) -> Result<Option<RuleOutput>, RuleError> {
        let rule = LuaRule::new("test.lua", src.to_string())?;
        let inp = input(telemetry);
        let cx =
            EvalCtx { input: &inp, mem: mem(store, "default"), limits: &limits, deadline: Instant::now() + budget };
        rule.evaluate(&cx)
    }

    const DEFAULT_RULE: &str = r#"
local combustivel = tonumber(pulse.telemetry["fuel"]) or 0
if combustivel < 70 then
   return { action = "SHUTDOWN", cmd_name = "EMERGENCY_CUTOFF", priority = 1000, params = { ["reason"] = "Overheating" } }
end
"#;

    #[test]
    fn shipped_default_rule_behaves_like_before() {
        let out = run(DEFAULT_RULE, &[("fuel", "53")]).unwrap().unwrap();
        assert_eq!(out.action, "SHUTDOWN");
        assert_eq!(out.cmd_name.as_deref(), Some("EMERGENCY_CUTOFF"));
        assert_eq!(out.priority, Some(1000));
        assert_eq!(out.params.unwrap()["reason"], "Overheating");
        assert!(run(DEFAULT_RULE, &[("fuel", "89")]).unwrap().is_none());
    }

    #[test]
    fn dangerous_globals_are_gone() {
        // PoC 3 regression: none of these may be reachable from a rule
        for expr in ["os", "io", "package", "debug", "dofile", "loadfile", "load", "require", "collectgarbage", "print"]
        {
            let src = format!("return {{ action = type({expr}) == 'nil' and 'IDLE' or 'SHUTDOWN' }}");
            assert_eq!(run(&src, &[]).unwrap().unwrap().action, "IDLE", "{expr} must not exist");
        }
        let err = run("os.execute('echo pwned')", &[]).unwrap_err();
        assert_eq!(err.kind, RuleErrorKind::Runtime);
    }

    #[test]
    fn infinite_loop_is_cut_off_by_the_instruction_budget() {
        // PoC 4 regression
        let limits = Limits { lua_instructions: 200_000, ..Limits::default() };
        let t = Instant::now();
        let err = run_with("while true do end", &[], limits, Duration::from_secs(30)).unwrap_err();
        assert_eq!(err.kind, RuleErrorKind::Budget, "{err}");
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn wall_clock_deadline_also_applies() {
        let limits = Limits { lua_instructions: u64::MAX / 2, ..Limits::default() };
        let err = run_with("while true do end", &[], limits, Duration::from_millis(50)).unwrap_err();
        assert_eq!(err.kind, RuleErrorKind::Timeout, "{err}");
    }

    #[test]
    fn memory_bomb_is_contained() {
        let limits = Limits { lua_memory_bytes: 2 * 1024 * 1024, ..Limits::default() };
        let err =
            run_with("local s = string.rep('x', 1e9) return nil", &[], limits, Duration::from_secs(5)).unwrap_err();
        assert_eq!(err.kind, RuleErrorKind::Budget, "{err}");
    }

    #[test]
    fn syntax_errors_are_caught_at_load_not_run() {
        let e = LuaRule::new("bad.lua", "if then".into()).err().unwrap();
        assert!(e.message.contains("syntax error"), "{e}");
        // and loading never executes the script (the original validator ran it without `pulse`)
        LuaRule::new("ok.lua", "local x = pulse.telemetry['fuel'] return nil".into()).unwrap();
    }

    #[test]
    fn redb_api_get_on_missing_key_is_nil() {
        // PoC: wiki example panicked the worker here
        let out = run("local v = redb_api.get('nope') return { action = v == nil and 'IDLE' or 'SHUTDOWN' }", &[])
            .unwrap()
            .unwrap();
        assert_eq!(out.action, "IDLE");
    }

    #[test]
    fn wiki_persistence_example_works_across_pulses() {
        let src = r#"
local temp = tonumber(pulse.telemetry["cpu_temp"]) or 0
local key = pulse.agent_id .. "_overheat_strikes"
local strikes = tonumber(redb_api.get(key)) or 0
if temp > 90 then
    strikes = strikes + 1
    redb_api.set(key, strikes)
    if strikes >= 3 then
        redb_api.set(key, 0)
        return { action = "SHUTDOWN", cmd_name = "PERSISTENT_OVERHEAT", priority = 1000, params = { ["msg"] = "hot" } }
    end
else
    if strikes > 0 then redb_api.set(key, 0) end
end
return { action = "IDLE", priority = 0 }
"#;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let go = |t: &str| {
            run_on(&store, src, &[("cpu_temp", t)], Limits::default(), Duration::from_secs(5)).unwrap().unwrap().action
        };
        assert_eq!(go("95"), "IDLE");
        assert_eq!(go("95"), "IDLE");
        assert_eq!(go("95"), "SHUTDOWN");
        assert_eq!(go("95"), "IDLE", "counter was reset after shutdown");
    }

    #[test]
    fn incr_and_del_helpers() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let src = "local a = redb_api.incr('c') local b = redb_api.incr('c', 4) redb_api.del('gone') return { action = 'CUSTOM', params = { a = a, b = b } }";
        let out = run_on(&store, src, &[], Limits::default(), Duration::from_secs(5)).unwrap().unwrap();
        let p = out.params.unwrap();
        assert_eq!((p["a"].as_str(), p["b"].as_str()), ("1", "5"));
    }

    #[test]
    fn negative_or_fractional_values_are_rejected() {
        assert!(run("redb_api.set('k', -1)", &[]).is_err());
        assert!(run("redb_api.set('k', 1.5)", &[]).is_err());
        assert!(run("redb_api.set('k', 2.0) return nil", &[]).is_ok());
    }

    #[test]
    fn deterministic_math_random() {
        let a = run("return { action = 'CUSTOM', params = { r = math.random(1, 1000000) } }", &[]).unwrap().unwrap();
        let b = run("return { action = 'CUSTOM', params = { r = math.random(1, 1000000) } }", &[]).unwrap().unwrap();
        assert_eq!(a.params, b.params, "same pulse must give the same decision");
    }

    #[test]
    fn contract_violations_are_reported() {
        assert_eq!(run("return { priority = 5 }", &[]).unwrap_err().kind, RuleErrorKind::Contract);
        assert_eq!(run("return 42", &[]).unwrap_err().kind, RuleErrorKind::Contract);
    }

    #[test]
    fn pulse_exposes_tenant_and_timestamp() {
        let out = run("return { action = 'CUSTOM', cmd_name = pulse.tenant, params = { ts = pulse.timestamp } }", &[])
            .unwrap()
            .unwrap();
        assert_eq!(out.cmd_name.as_deref(), Some("default"));
        assert_eq!(out.params.unwrap()["ts"], "1700000000");
    }
}
