use serde::Serialize;

#[derive(Serialize, Debug)]
pub struct SysJson
{
    pub cpu_usage: f32,
    pub used_mem: f64,
    pub total_mem: f64,
}