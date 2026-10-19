//! Deployment runtime detection: bare daemon, plain Docker, Docker Swarm or Kubernetes.
//!
//! The runtime only changes *defaults* (bind address, log destination, whether
//! authentication is mandatory, data directory) and how peers are refreshed. The Raft
//! protocol and the rule engine behave identically everywhere.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    Daemon,
    Docker,
    Swarm,
    Kubernetes,
}

impl Runtime {
    /// Detect from the real environment.
    pub fn detect() -> Self {
        Self::detect_with(
            |k| std::env::var(k).ok(),
            |p| Path::new(p).exists(),
            || std::fs::read_to_string("/proc/1/cgroup").ok(),
        )
    }

    /// Testable detection. `env` reads environment variables, `exists` probes the
    /// filesystem, `cgroup` returns the content of `/proc/1/cgroup` when available.
    pub fn detect_with(
        env: impl Fn(&str) -> Option<String>,
        exists: impl Fn(&str) -> bool,
        cgroup: impl Fn() -> Option<String>,
    ) -> Self {
        if let Some(v) = env("CARDINAL_RUNTIME")
            && let Ok(r) = v.parse()
        {
            return r;
        }
        if env("KUBERNETES_SERVICE_HOST").is_some() || exists("/var/run/secrets/kubernetes.io/serviceaccount") {
            return Runtime::Kubernetes;
        }
        // Swarm injects no marker of its own; the stack file templates one in
        // (`CARDINAL_SWARM_SERVICE: "{{.Service.Name}}"`).
        if env("CARDINAL_SWARM_SERVICE").is_some_and(|v| !v.is_empty()) {
            return Runtime::Swarm;
        }
        let in_container = exists("/.dockerenv")
            || exists("/run/.containerenv")
            || cgroup().is_some_and(|c| c.contains("docker") || c.contains("containerd") || c.contains("kubepods"));
        if in_container {
            return Runtime::Docker;
        }
        Runtime::Daemon
    }

    /// Container runtimes cannot be reached through the loopback of the host, so they
    /// default to binding all interfaces (and therefore to mandatory authentication).
    pub fn is_container(self) -> bool {
        !matches!(self, Runtime::Daemon)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Runtime::Daemon => "daemon",
            Runtime::Docker => "docker",
            Runtime::Swarm => "swarm",
            Runtime::Kubernetes => "kubernetes",
        }
    }
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Runtime {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "daemon" | "bare" | "host" => Ok(Runtime::Daemon),
            "docker" => Ok(Runtime::Docker),
            "swarm" | "docker-swarm" => Ok(Runtime::Swarm),
            "kubernetes" | "k8s" => Ok(Runtime::Kubernetes),
            other => Err(format!("unknown runtime '{other}' (expected daemon|docker|swarm|kubernetes)")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn detect(env: &[(&str, &str)], files: &[&str], cgroup: Option<&str>) -> Runtime {
        let env: HashMap<String, String> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let files: Vec<String> = files.iter().map(|s| s.to_string()).collect();
        let cg = cgroup.map(str::to_string);
        Runtime::detect_with(|k| env.get(k).cloned(), |p| files.iter().any(|f| f == p), || cg.clone())
    }

    #[test]
    fn bare_host_is_daemon() {
        assert_eq!(detect(&[], &[], None), Runtime::Daemon);
    }

    #[test]
    fn kubernetes_wins_over_docker_markers() {
        let r = detect(&[("KUBERNETES_SERVICE_HOST", "10.0.0.1")], &["/.dockerenv"], None);
        assert_eq!(r, Runtime::Kubernetes);
        let r = detect(&[], &["/var/run/secrets/kubernetes.io/serviceaccount"], None);
        assert_eq!(r, Runtime::Kubernetes);
    }

    #[test]
    fn swarm_needs_the_templated_marker() {
        let r = detect(&[("CARDINAL_SWARM_SERVICE", "cardinal")], &["/.dockerenv"], None);
        assert_eq!(r, Runtime::Swarm);
        assert_eq!(detect(&[], &["/.dockerenv"], None), Runtime::Docker);
    }

    #[test]
    fn docker_from_cgroup() {
        assert_eq!(detect(&[], &[], Some("0::/docker/abc")), Runtime::Docker);
    }

    #[test]
    fn explicit_override_wins() {
        assert_eq!(detect(&[("CARDINAL_RUNTIME", "swarm")], &["/.dockerenv"], None), Runtime::Swarm);
        assert_eq!(detect(&[("CARDINAL_RUNTIME", "bogus")], &[], None), Runtime::Daemon);
    }
}
