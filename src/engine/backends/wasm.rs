//! WebAssembly rules (wasmi interpreter, fuel metered).
//!
//! Why an interpreter: rule execution must be deterministic and strictly bounded. Every
//! instruction costs fuel (so a rule can never spin), linear memory is capped, there is no
//! WASI (no files, clock, network or randomness) and the only door to the outside world is
//! the small `cardinal` host API below.
//!
//! ## Guest ABI
//!
//! Exports:
//! * `memory` — linear memory
//! * `cardinal_alloc(size: i32) -> i32` — returns a pointer to `size` writable bytes
//! * `cardinal_evaluate(ptr: i32, len: i32) -> i64` — receives the pulse as JSON
//!   (`{"agent_id","tenant","timestamp","trace_id","telemetry":{...}}`) and returns
//!   `(out_ptr << 32) | out_len` pointing at a JSON rule output
//!   (`{"action","cmd_name","params","priority"}`), or `0` for "no opinion"
//!
//! Imports (module `cardinal`, all optional):
//! * `kv_get(key_ptr, key_len, out_ptr) -> i32` — writes the `u64` (LE) at `out_ptr`; 1 found, 0 absent
//! * `kv_set(key_ptr, key_len, value: i64) -> i32` — 0 ok
//! * `kv_add(key_ptr, key_len, delta: i64, out_ptr) -> i32` — atomic add, writes the new `u64`; 0 ok
//! * `kv_del(key_ptr, key_len) -> i32` — 1 deleted, 0 absent
//! * `log(ptr, len)` — diagnostic line
//!
//! Negative return values: -1 bad pointer, -2 budget/quota, -3 storage error, -4 bad key.

use wasmi::{Caller, Config, Engine, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, TrapCode};

use crate::engine::mem::SharedMem;
use crate::engine::types::{EvalCtx, MAX_KEY_LEN, RuleBackend, RuleError, RuleErrorKind, RuleKind, RuleOutput};

const E_MEM: i32 = -1;
const E_BUDGET: i32 = -2;
const E_HOST: i32 = -3;
const E_KEY: i32 = -4;

const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_LOG_CALLS: u32 = 16;
const MAX_LOG_BYTES: usize = 512;
const KNOWN_IMPORTS: [&str; 5] = ["kv_get", "kv_set", "kv_add", "kv_del", "log"];

struct HostState {
    mem: std::sync::Arc<SharedMem>,
    limits: StoreLimits,
    tenant: String,
    agent: String,
    logs: u32,
}

pub struct WasmRule {
    name: String,
    engine: Engine,
    module: Module,
    linker: Linker<HostState>,
}

impl WasmRule {
    /// Compile and validate. Rejects modules that miss the ABI or import anything outside
    /// the `cardinal` host API.
    pub fn new(name: &str, bytes: &[u8]) -> Result<Self, RuleError> {
        let mut cfg = Config::default();
        cfg.consume_fuel(true);
        cfg.allow_start_fn(false);
        cfg.enforced_limits(wasmi::EnforcedLimits::strict());
        let engine = Engine::new(&cfg);
        let module =
            Module::new(&engine, bytes).map_err(|e| RuleError::runtime(format!("invalid wasm module: {e}")))?;

        for imp in module.imports() {
            let ok = imp.module() == "cardinal" && KNOWN_IMPORTS.contains(&imp.name());
            if !ok {
                return Err(RuleError::contract(format!(
                    "module imports `{}::{}`, which is not part of the cardinal host API",
                    imp.module(),
                    imp.name()
                )));
            }
        }
        for export in ["memory", "cardinal_alloc", "cardinal_evaluate"] {
            if module.get_export(export).is_none() {
                return Err(RuleError::contract(format!("module does not export `{export}`")));
            }
        }

        let mut linker = <Linker<HostState>>::new(&engine);
        define_host_api(&mut linker).map_err(|e| RuleError::runtime(format!("linker: {e}")))?;
        Ok(Self { name: name.to_string(), engine, module, linker })
    }
}

fn get_memory(caller: &Caller<'_, HostState>) -> Option<Memory> {
    caller.get_export("memory").and_then(|e| e.into_memory())
}

fn read_key(caller: &Caller<'_, HostState>, ptr: i32, len: i32) -> Result<String, i32> {
    if len < 0 || len as usize > MAX_KEY_LEN || ptr < 0 {
        return Err(E_KEY);
    }
    let memory = get_memory(caller).ok_or(E_MEM)?;
    let mut buf = vec![0u8; len as usize];
    memory.read(caller, ptr as usize, &mut buf).map_err(|_| E_MEM)?;
    String::from_utf8(buf).map_err(|_| E_KEY)
}

fn write_u64(caller: &mut Caller<'_, HostState>, ptr: i32, v: u64) -> Result<(), i32> {
    if ptr < 0 {
        return Err(E_MEM);
    }
    let memory = get_memory(caller).ok_or(E_MEM)?;
    memory.write(caller, ptr as usize, &v.to_le_bytes()).map_err(|_| E_MEM)
}

fn host_code(e: &RuleError) -> i32 {
    match e.kind {
        RuleErrorKind::Budget => E_BUDGET,
        RuleErrorKind::Runtime => E_KEY,
        _ => E_HOST,
    }
}

fn define_host_api(linker: &mut Linker<HostState>) -> Result<(), wasmi::Error> {
    linker.func_wrap("cardinal", "kv_get", |mut c: Caller<'_, HostState>, kp: i32, kl: i32, out: i32| -> i32 {
        let key = match read_key(&c, kp, kl) {
            Ok(k) => k,
            Err(e) => return e,
        };
        match c.data().mem.get(&key) {
            Ok(Some(v)) => write_u64(&mut c, out, v).map(|_| 1).unwrap_or_else(|e| e),
            Ok(None) => 0,
            Err(e) => host_code(&e),
        }
    })?;
    linker.func_wrap("cardinal", "kv_set", |c: Caller<'_, HostState>, kp: i32, kl: i32, value: i64| -> i32 {
        let key = match read_key(&c, kp, kl) {
            Ok(k) => k,
            Err(e) => return e,
        };
        if value < 0 {
            return E_KEY;
        }
        c.data().mem.set(&key, value as u64).map(|_| 0).unwrap_or_else(|e| host_code(&e))
    })?;
    linker.func_wrap(
        "cardinal",
        "kv_add",
        |mut c: Caller<'_, HostState>, kp: i32, kl: i32, delta: i64, out: i32| -> i32 {
            let key = match read_key(&c, kp, kl) {
                Ok(k) => k,
                Err(e) => return e,
            };
            match c.data().mem.add(&key, delta) {
                Ok(v) => write_u64(&mut c, out, v).map(|_| 0).unwrap_or_else(|e| e),
                Err(e) => host_code(&e),
            }
        },
    )?;
    linker.func_wrap("cardinal", "kv_del", |c: Caller<'_, HostState>, kp: i32, kl: i32| -> i32 {
        let key = match read_key(&c, kp, kl) {
            Ok(k) => k,
            Err(e) => return e,
        };
        c.data().mem.delete(&key).map(i32::from).unwrap_or_else(|e| host_code(&e))
    })?;
    linker.func_wrap("cardinal", "log", |mut c: Caller<'_, HostState>, ptr: i32, len: i32| {
        if c.data().logs >= MAX_LOG_CALLS || len < 0 || ptr < 0 {
            return;
        }
        c.data_mut().logs += 1;
        let n = (len as usize).min(MAX_LOG_BYTES);
        let Some(memory) = get_memory(&c) else { return };
        let mut buf = vec![0u8; n];
        if memory.read(&c, ptr as usize, &mut buf).is_ok() {
            let msg = String::from_utf8_lossy(&buf);
            let st = c.data();
            tracing::info!(tenant = %st.tenant, agent = %crate::util::log_safe(&st.agent), "wasm rule: {}", crate::util::log_safe(&msg));
        }
    })?;
    Ok(())
}

fn classify(e: &wasmi::Error) -> RuleError {
    match e.as_trap_code() {
        Some(TrapCode::OutOfFuel) => RuleError::new(RuleErrorKind::Budget, "fuel exhausted"),
        Some(code) => RuleError::runtime(format!("trap: {code}")),
        None => RuleError::runtime(e.to_string()),
    }
}

impl RuleBackend for WasmRule {
    fn kind(&self) -> RuleKind {
        RuleKind::Wasm
    }

    fn evaluate(&self, cx: &EvalCtx<'_>) -> Result<Option<RuleOutput>, RuleError> {
        let input = serde_json::to_vec(cx.input).map_err(RuleError::runtime)?;
        let limits = StoreLimitsBuilder::new()
            .memory_size(cx.limits.wasm_memory_bytes)
            .memories(1)
            .tables(4)
            .instances(1)
            .trap_on_grow_failure(false)
            .build();
        let state = HostState {
            mem: cx.mem.clone(),
            limits,
            tenant: cx.input.tenant.clone(),
            agent: cx.input.agent_id.clone(),
            logs: 0,
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);
        store.set_fuel(cx.limits.wasm_fuel).map_err(|e| RuleError::runtime(e.to_string()))?;

        let instance = self.linker.instantiate_and_start(&mut store, &self.module).map_err(|e| classify(&e))?;
        let memory = instance.get_memory(&store, "memory").ok_or_else(|| RuleError::contract("no exported memory"))?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&store, "cardinal_alloc")
            .map_err(|e| RuleError::contract(format!("cardinal_alloc: {e}")))?;
        let evaluate = instance
            .get_typed_func::<(i32, i32), i64>(&store, "cardinal_evaluate")
            .map_err(|e| RuleError::contract(format!("cardinal_evaluate: {e}")))?;

        let in_len = i32::try_from(input.len()).map_err(|_| RuleError::contract("input too large"))?;
        let in_ptr = alloc.call(&mut store, in_len).map_err(|e| classify(&e))?;
        if in_ptr < 0 {
            return Err(RuleError::contract("cardinal_alloc returned a negative pointer"));
        }
        memory
            .write(&mut store, in_ptr as usize, &input)
            .map_err(|e| RuleError::contract(format!("cardinal_alloc returned an invalid buffer: {e}")))?;

        let packed = evaluate.call(&mut store, (in_ptr, in_len)).map_err(|e| classify(&e))?;
        if std::time::Instant::now() >= cx.deadline {
            return Err(RuleError::new(RuleErrorKind::Timeout, "evaluation deadline exceeded"));
        }
        if packed == 0 {
            return Ok(None);
        }
        let out_ptr = ((packed as u64) >> 32) as usize;
        let out_len = ((packed as u64) & 0xffff_ffff) as usize;
        if out_len == 0 || out_len > MAX_OUTPUT_BYTES {
            return Err(RuleError::contract(format!("output length {out_len} is outside 1..={MAX_OUTPUT_BYTES}")));
        }
        let mut out = vec![0u8; out_len];
        memory
            .read(&store, out_ptr, &mut out)
            .map_err(|e| RuleError::contract(format!("output pointer is out of bounds: {e}")))?;
        serde_json::from_slice::<RuleOutput>(&out)
            .map(Some)
            .map_err(|e| RuleError::contract(format!("{}: invalid output JSON: {e}", self.name)))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::Limits;
    use crate::engine::mem::testing::mem;
    use crate::engine::types::PulseInput;
    use crate::store::Store as KvStore;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    pub(crate) const LOW_FUEL_OUT: &str =
        r#"{"action":"SHUTDOWN","cmd_name":"EMERGENCY_CUTOFF","priority":1000,"params":{"reason":"Overheating"}}"#;

    /// Guest that parses `"fuel":"<n>"` out of the pulse and shuts down below 70 — the WASM
    /// twin of the shipped default Lua rule.
    pub(crate) fn low_fuel_wat() -> String {
        LOW_FUEL_TEMPLATE
            .replace("@@OUT@@", &LOW_FUEL_OUT.replace('"', "\\\""))
            .replace("@@LEN@@", &LOW_FUEL_OUT.len().to_string())
    }

    const LOW_FUEL_TEMPLATE: &str = r#"
(module
  (memory (export "memory") 1)
  (data (i32.const 1024) "\"fuel\":\"")
  (data (i32.const 2048) "@@OUT@@")
  (func (export "cardinal_alloc") (param i32) (result i32) (i32.const 8192))
  ;; returns index just past the first occurrence of the 8-byte needle at 1024, or -1
  (func $find (param $ptr i32) (param $len i32) (result i32)
    (local $i i32) (local $j i32)
    (block $notfound
      (loop $outer
        (br_if $notfound (i32.gt_s (i32.add (local.get $i) (i32.const 8)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch
              (i32.ne
                (i32.load8_u (i32.add (local.get $ptr) (i32.add (local.get $i) (local.get $j))))
                (i32.load8_u (i32.add (i32.const 1024) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (i32.const 8)))
            (return (i32.add (local.get $i) (i32.const 8)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const -1))
  (func (export "cardinal_evaluate") (param $ptr i32) (param $len i32) (result i64)
    (local $p i32) (local $v i32) (local $c i32)
    (local.set $p (call $find (local.get $ptr) (local.get $len)))
    (if (i32.lt_s (local.get $p) (i32.const 0)) (then (return (i64.const 0))))
    (block $done
      (loop $digits
        (local.set $c (i32.load8_u (i32.add (local.get $ptr) (local.get $p))))
        (br_if $done (i32.or (i32.lt_u (local.get $c) (i32.const 48)) (i32.gt_u (local.get $c) (i32.const 57))))
        (local.set $v (i32.add (i32.mul (local.get $v) (i32.const 10)) (i32.sub (local.get $c) (i32.const 48))))
        (local.set $p (i32.add (local.get $p) (i32.const 1)))
        (br $digits)))
    (if (i32.lt_u (local.get $v) (i32.const 70))
      (then (return (i64.or (i64.shl (i64.const 2048) (i64.const 32)) (i64.const @@LEN@@)))))
    (i64.const 0)))
"#;

    fn input(telemetry: &[(&str, &str)]) -> PulseInput {
        PulseInput {
            agent_id: "Rocket_01".into(),
            tenant: "default".into(),
            timestamp: 1,
            trace_id: "t".into(),
            telemetry: telemetry.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>(),
        }
    }

    fn run(wat_src: &str, telemetry: &[(&str, &str)], limits: Limits) -> Result<Option<RuleOutput>, RuleError> {
        let store = Arc::new(KvStore::open_in_memory().unwrap());
        run_on(&store, wat_src, telemetry, limits)
    }

    fn run_on(
        store: &Arc<KvStore>,
        wat_src: &str,
        telemetry: &[(&str, &str)],
        limits: Limits,
    ) -> Result<Option<RuleOutput>, RuleError> {
        let wasm = wat::parse_str(wat_src).unwrap();
        let rule = WasmRule::new("t", &wasm)?;
        let inp = input(telemetry);
        let cx = EvalCtx {
            input: &inp,
            mem: mem(store, "default"),
            limits: &limits,
            deadline: Instant::now() + Duration::from_secs(5),
        };
        rule.evaluate(&cx)
    }

    /// `(module ...)` with a fixed JSON answer.
    fn constant(json: &str) -> String {
        let esc = json.replace('"', "\\\"");
        format!(
            r#"(module (memory (export "memory") 1)
              (data (i32.const 1024) "{esc}")
              (func (export "cardinal_alloc") (param i32) (result i32) (i32.const 8192))
              (func (export "cardinal_evaluate") (param i32 i32) (result i64)
                (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const {len}))))"#,
            len = json.len()
        )
    }

    #[test]
    fn guest_reads_telemetry_and_decides() {
        let out = run(&low_fuel_wat(), &[("fuel", "53")], Limits::default()).unwrap().unwrap();
        assert_eq!(out.action, "SHUTDOWN");
        assert_eq!(out.cmd_name.as_deref(), Some("EMERGENCY_CUTOFF"));
        assert_eq!(out.priority, Some(1000));
        assert_eq!(out.params.unwrap()["reason"], "Overheating");
        assert!(run(&low_fuel_wat(), &[("fuel", "89")], Limits::default()).unwrap().is_none());
        assert!(run(&low_fuel_wat(), &[], Limits::default()).unwrap().is_none(), "no fuel key => no opinion");
    }

    #[test]
    fn constant_guest() {
        let out = run(&constant(r#"{"action":"RESTART","priority":7}"#), &[], Limits::default()).unwrap().unwrap();
        assert_eq!((out.action.as_str(), out.priority), ("RESTART", Some(7)));
    }

    #[test]
    fn infinite_loop_runs_out_of_fuel() {
        let looping = r#"(module (memory (export "memory") 1)
            (func (export "cardinal_alloc") (param i32) (result i32) (i32.const 8192))
            (func (export "cardinal_evaluate") (param i32 i32) (result i64) (loop $l (br $l)) (i64.const 0)))"#;
        let limits = Limits { wasm_fuel: 100_000, ..Limits::default() };
        let t = Instant::now();
        let err = run(looping, &[], limits).unwrap_err();
        assert_eq!(err.kind, RuleErrorKind::Budget, "{err}");
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn memory_growth_is_capped() {
        // asks for 1000 pages (64 MiB) with a 1 MiB cap: grow must fail (-1), and the guest reports it
        let grow = r#"(module (memory (export "memory") 1)
            (data (i32.const 1024) "{\"action\":\"CUSTOM\",\"priority\":1}")
            (func (export "cardinal_alloc") (param i32) (result i32) (i32.const 8192))
            (func (export "cardinal_evaluate") (param i32 i32) (result i64)
              (if (i32.ne (memory.grow (i32.const 1000)) (i32.const -1)) (then unreachable))
              (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const 32))))"#;
        let limits = Limits { wasm_memory_bytes: 1024 * 1024, ..Limits::default() };
        assert!(run(grow, &[], limits).unwrap().is_some(), "grow was refused, guest saw -1");
    }

    #[test]
    fn host_api_is_tenant_scoped_shared_memory() {
        let wat_src = r#"(module
          (import "cardinal" "kv_add" (func $kv_add (param i32 i32 i64 i32) (result i32)))
          (memory (export "memory") 1)
          (data (i32.const 1024) "hits")
          (data (i32.const 2048) "{\"action\":\"CUSTOM\",\"priority\":1}")
          (func (export "cardinal_alloc") (param i32) (result i32) (i32.const 8192))
          (func (export "cardinal_evaluate") (param i32 i32) (result i64)
            (drop (call $kv_add (i32.const 1024) (i32.const 4) (i64.const 1) (i32.const 4096)))
            (i64.or (i64.shl (i64.const 2048) (i64.const 32)) (i64.const 32))))"#;
        let store = Arc::new(KvStore::open_in_memory().unwrap());
        run_on(&store, wat_src, &[], Limits::default()).unwrap();
        run_on(&store, wat_src, &[], Limits::default()).unwrap();
        assert_eq!(store.kv_get("default", "hits").unwrap(), Some(2));
        assert_eq!(store.kv_get("other", "hits").unwrap(), None);
    }

    #[test]
    fn imports_outside_the_host_api_are_refused() {
        for imp in [
            r#"(import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32)))"#,
            r#"(import "env" "abort" (func))"#,
            r#"(import "cardinal" "exec" (func))"#,
        ] {
            let wasm = wat::parse_str(format!(
                r#"(module {imp} (memory (export "memory") 1)
                  (func (export "cardinal_alloc") (param i32) (result i32) (i32.const 0))
                  (func (export "cardinal_evaluate") (param i32 i32) (result i64) (i64.const 0)))"#
            ))
            .unwrap();
            let e = WasmRule::new("x", &wasm).err().expect("must be rejected");
            assert_eq!(e.kind, RuleErrorKind::Contract, "{e}");
        }
    }

    #[test]
    fn missing_abi_exports_are_refused() {
        let wasm = wat::parse_str(r#"(module (memory (export "memory") 1))"#).unwrap();
        let e = WasmRule::new("x", &wasm).err().unwrap();
        assert!(e.message.contains("cardinal_alloc") || e.message.contains("cardinal_evaluate"), "{e}");
        assert!(WasmRule::new("x", b"not wasm").is_err());
    }

    #[test]
    fn start_functions_are_refused() {
        let wasm = wat::parse_str(
            r#"(module (memory (export "memory") 1)
            (func $s (loop $l (br $l))) (start $s)
            (func (export "cardinal_alloc") (param i32) (result i32) (i32.const 0))
            (func (export "cardinal_evaluate") (param i32 i32) (result i64) (i64.const 0)))"#,
        )
        .unwrap();
        assert!(WasmRule::new("x", &wasm).is_err());
    }

    #[test]
    fn out_of_bounds_output_pointer_is_a_contract_error() {
        let bad = r#"(module (memory (export "memory") 1)
            (func (export "cardinal_alloc") (param i32) (result i32) (i32.const 8192))
            (func (export "cardinal_evaluate") (param i32 i32) (result i64)
              (i64.or (i64.shl (i64.const 70000) (i64.const 32)) (i64.const 10))))"#;
        assert_eq!(run(bad, &[], Limits::default()).unwrap_err().kind, RuleErrorKind::Contract);
    }

    #[test]
    fn garbage_output_is_a_contract_error() {
        let e = run(&constant("not json"), &[], Limits::default()).unwrap_err();
        assert_eq!(e.kind, RuleErrorKind::Contract);
    }
}
