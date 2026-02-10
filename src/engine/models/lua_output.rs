use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct LuaOutput {
    pub action: String,
    pub cmd_name: Option<String>,
    pub params: Option<HashMap<String, String>>,
    pub priority: Option<i32>,
}
