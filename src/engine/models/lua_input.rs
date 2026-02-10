use serde::Serialize;
use std::collections::HashMap;

#[derive(Serialize)]
pub struct LuaInput<'a> {
    pub agent_id: &'a str,
    pub telemetry: &'a HashMap<String, String>,
}