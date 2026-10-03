//! Process-wide counters, rendered as legacy JSON or Prometheus text.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

/// Upper bounds (µs) of the evaluation latency histogram.
const BUCKETS_US: [u64; 12] = [100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 1_000_000];

#[derive(Default)]
pub struct Metrics {
    pub pulses: AtomicU64,
    pub rejected: AtomicU64,
    pub forced: AtomicU64,
    pub rule_errors: AtomicU64,
    /// Pulses being processed right now (the legacy `connected_agents` value).
    pub in_flight: AtomicUsize,
    actions: [AtomicU64; 4],
    latency_buckets: [AtomicU64; BUCKETS_US.len() + 1],
    latency_sum_us: AtomicU64,
    tenants: Mutex<BTreeMap<String, TenantCounters>>,
}

#[derive(Default, Clone, Debug)]
pub struct TenantCounters {
    pub pulses: u64,
    pub rejected: u64,
    pub rate_limited: u64,
    pub rule_errors: u64,
    pub actions: [u64; 4],
}

/// RAII guard: the in-flight counter can never leak, whatever path the request takes
/// (the original code skipped the decrement on every early `?` return).
pub struct InFlight<'a>(&'a Metrics);

impl Metrics {
    pub fn enter(&self) -> InFlight<'_> {
        self.in_flight.fetch_add(1, Relaxed);
        InFlight(self)
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Relaxed)
    }

    pub fn observe_pulse(&self, tenant: &str, action: i32, micros: u64, rule_errors: u64) {
        self.pulses.fetch_add(1, Relaxed);
        let a = (action.clamp(0, 3)) as usize;
        self.actions[a].fetch_add(1, Relaxed);
        self.rule_errors.fetch_add(rule_errors, Relaxed);
        self.latency_sum_us.fetch_add(micros, Relaxed);
        let idx = BUCKETS_US.iter().position(|&b| micros <= b).unwrap_or(BUCKETS_US.len());
        self.latency_buckets[idx].fetch_add(1, Relaxed);
        self.with_tenant(tenant, |t| {
            t.pulses += 1;
            t.actions[a] += 1;
            t.rule_errors += rule_errors;
        });
    }

    pub fn observe_rejected(&self, tenant: Option<&str>, rate_limited: bool) {
        self.rejected.fetch_add(1, Relaxed);
        if let Some(t) = tenant {
            self.with_tenant(t, |c| {
                c.rejected += 1;
                if rate_limited {
                    c.rate_limited += 1;
                }
            });
        }
    }

    fn with_tenant(&self, tenant: &str, f: impl FnOnce(&mut TenantCounters)) {
        let mut m = self.tenants.lock().unwrap_or_else(|p| p.into_inner());
        if !m.contains_key(tenant) {
            // bounded: tenants come from the registry, never from request data
            m.insert(tenant.to_string(), TenantCounters::default());
        }
        f(m.get_mut(tenant).expect("just inserted"));
    }

    pub fn tenant_counters(&self) -> BTreeMap<String, TenantCounters> {
        self.tenants.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Approximate percentile (upper bound of the bucket) in µs.
    pub fn latency_quantile_us(&self, q: f64) -> u64 {
        let total: u64 = self.latency_buckets.iter().map(|b| b.load(Relaxed)).sum();
        if total == 0 {
            return 0;
        }
        let target = (total as f64 * q).ceil() as u64;
        let mut acc = 0;
        for (i, b) in self.latency_buckets.iter().enumerate() {
            acc += b.load(Relaxed);
            if acc >= target {
                return BUCKETS_US.get(i).copied().unwrap_or(u64::MAX);
            }
        }
        u64::MAX
    }

    /// Prometheus text exposition. `extra` lets other subsystems (raft, rules) add gauges.
    pub fn render_prometheus(&self, extra: &[(&str, &str, f64)]) -> String {
        let mut o = String::with_capacity(2048);
        let counter = |o: &mut String, name: &str, help: &str, v: u64| {
            let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}");
        };
        counter(&mut o, "cardinal_pulses_total", "Pulses decided.", self.pulses.load(Relaxed));
        counter(
            &mut o,
            "cardinal_pulses_rejected_total",
            "Pulses refused (auth, rate limit, validation).",
            self.rejected.load(Relaxed),
        );
        counter(
            &mut o,
            "cardinal_forced_reactions_total",
            "Pulses answered by a Heathcliff override.",
            self.forced.load(Relaxed),
        );
        counter(&mut o, "cardinal_rule_errors_total", "Rule evaluations that failed.", self.rule_errors.load(Relaxed));

        let _ = writeln!(
            o,
            "# HELP cardinal_in_flight_pulses Pulses being processed.\n# TYPE cardinal_in_flight_pulses gauge\ncardinal_in_flight_pulses {}",
            self.in_flight()
        );

        let _ = writeln!(
            o,
            "# HELP cardinal_reactions_total Reactions by action.\n# TYPE cardinal_reactions_total counter"
        );
        for (i, name) in ["idle", "shutdown", "restart", "custom"].iter().enumerate() {
            let _ = writeln!(o, "cardinal_reactions_total{{action=\"{name}\"}} {}", self.actions[i].load(Relaxed));
        }

        let _ = writeln!(
            o,
            "# HELP cardinal_tenant_pulses_total Pulses decided per tenant.\n# TYPE cardinal_tenant_pulses_total counter"
        );
        let tenants = self.tenant_counters();
        for (t, c) in &tenants {
            let _ = writeln!(o, "cardinal_tenant_pulses_total{{tenant=\"{t}\"}} {}", c.pulses);
        }
        let _ = writeln!(
            o,
            "# HELP cardinal_tenant_rejected_total Rejected pulses per tenant.\n# TYPE cardinal_tenant_rejected_total counter"
        );
        for (t, c) in &tenants {
            let _ = writeln!(o, "cardinal_tenant_rejected_total{{tenant=\"{t}\"}} {}", c.rejected);
        }

        let _ = writeln!(
            o,
            "# HELP cardinal_decision_latency_us Decision latency.\n# TYPE cardinal_decision_latency_us histogram"
        );
        let mut acc = 0;
        for (i, ub) in BUCKETS_US.iter().enumerate() {
            acc += self.latency_buckets[i].load(Relaxed);
            let _ = writeln!(o, "cardinal_decision_latency_us_bucket{{le=\"{ub}\"}} {acc}");
        }
        acc += self.latency_buckets[BUCKETS_US.len()].load(Relaxed);
        let _ = writeln!(o, "cardinal_decision_latency_us_bucket{{le=\"+Inf\"}} {acc}");
        let _ = writeln!(o, "cardinal_decision_latency_us_sum {}", self.latency_sum_us.load(Relaxed));
        let _ = writeln!(o, "cardinal_decision_latency_us_count {acc}");

        for (name, help, v) in extra {
            let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}");
        }
        o
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_returns_to_zero_even_on_early_exit() {
        let m = Metrics::default();
        fn fails(m: &Metrics) -> Result<(), ()> {
            let _g = m.enter();
            Err(())?;
            Ok(())
        }
        for _ in 0..100 {
            let _ = fails(&m);
        }
        assert_eq!(m.in_flight(), 0, "regression: the original counter leaked on every error path");
    }

    #[test]
    fn quantiles_and_prometheus_output() {
        let m = Metrics::default();
        for us in [50, 80, 90, 400, 9_000] {
            m.observe_pulse("default", 1, us, 0);
        }
        assert_eq!(m.latency_quantile_us(0.5), 100);
        assert_eq!(m.latency_quantile_us(1.0), 10_000);
        let text = m.render_prometheus(&[("cardinal_raft_term", "Raft term.", 3.0)]);
        assert!(text.contains("cardinal_pulses_total 5"));
        assert!(text.contains("cardinal_reactions_total{action=\"shutdown\"} 5"));
        assert!(text.contains("cardinal_decision_latency_us_bucket{le=\"+Inf\"} 5"));
        assert!(text.contains("cardinal_raft_term 3"));
    }
}
