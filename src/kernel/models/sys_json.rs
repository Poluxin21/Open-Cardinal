use serde::{Deserialize, Serialize};

#[derive(Serialize, Debug)]
pub struct SysJson
{
    pub kernel_version: Option<String>,
    pub cpu_usage: f32,
    pub used_mem: f64,
    pub total_mem: f64,
}


#[derive(Serialize, Debug)]
pub struct MetricsJson
{
    pub total_rules: i32,
    pub agents_detected: i32,
    pub connected_agents: i32,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ConfigJson
{
    pub grpc_port: i32,
    pub http_port: i32,
    pub db_file: String,
}
