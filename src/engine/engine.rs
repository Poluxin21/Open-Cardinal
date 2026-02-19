use std::path::{Path, PathBuf};
use tokio::fs;
use mlua::prelude::*;
use tracing::error;
use std::collections::HashMap;

use super::models::lua_input::LuaInput;
use super::models::lua_output::LuaOutput;

use crate::engine::storage::{read_db, write_db};
#[allow(non_snake_case)]
use crate::g_rpc::g_rpc::cardinal_core::{Pulse, Reaction};

pub struct RuleEngine;

impl RuleEngine {
    
    pub async fn process(pulse: &Pulse) -> Reaction {
        let safe_id = pulse.agent_id.replace("/", "").replace("\\", "").replace("..", "");
        let agent_dir = Path::new("rules").join(&safe_id);

        let target_dir = if fs::try_exists(&agent_dir).await.unwrap_or(false) {
            agent_dir
        } else {
            Path::new("rules").join("default")
        };

        let mut dir_entries = match fs::read_dir(&target_dir).await {
            Ok(entries) => entries,
            Err(_) => return Self::make_idle("DIR_NOT_FOUND"),
        };

       let mut scripts: Vec<(PathBuf, String)> = Vec::new();

        while let Ok(Some(entry)) = dir_entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("lua") {
                if let Ok(content) = fs::read_to_string(&path).await {
                    scripts.push((path, content));
                }
            }
        }
        
        let lua = Lua::new();

        if let Err(e) = Self::inject_redb_api(&lua).await {
            error!("Redb inject error {}", e);
        }
        
        let input = LuaInput { 
            agent_id: &pulse.agent_id, 
            telemetry: &pulse.telemetry 
        };
        
        let mut best_reaction = Self::make_idle("NO_ACTION");
        let mut max_priority = -1;

        for (path, script_content) in scripts {
            match Self::run_script(&lua, &script_content, &input).await {
                Ok(output) => {
                    let priority = output.priority.unwrap_or(0);
                    if priority > max_priority {
                        max_priority = priority;
                        best_reaction = Self::convert_to_proto(output);
                        if priority >= 1000 { break; }
                    }
                },
                Err(e) => tracing::error!("⚠️ Error on script {}: {}", path.display(), e),
            }
        }

        best_reaction
    }

    async fn run_script(lua: &Lua, script_content: &str, input: &LuaInput<'_>) -> LuaResult<LuaOutput> {
        let input_value = lua.to_value(input)?;
        
        let globals = lua.globals();
        globals.set("pulse", input_value)?;
        
        let value: mlua::Value = lua.load(script_content).eval_async().await?;
        
        let output: LuaOutput = lua.from_value(value)?;
        
        Ok(output)
    }

    fn convert_to_proto(out: LuaOutput) -> Reaction {
        let type_enum = match out.action.as_str() {
            "SHUTDOWN" => 1,
            "RESTART" => 2,
            "CUSTOM" => 3,
            _ => 0,
        };
        Reaction {
            trace_id: "multi-script".to_string(),
            r#type: type_enum,
            command_name: out.cmd_name.unwrap_or_default(),
            parameters: out.params.unwrap_or_default(),
        }
    }

    fn make_idle(msg: &str) -> Reaction {
        Reaction {
            trace_id: "idle".to_string(),
            r#type: 0,
            command_name: msg.to_string(),
            parameters: HashMap::new(),
        }
    }

    async fn inject_redb_api(lua: &Lua) -> LuaResult<()> {
        let redb_api = lua.create_table()?;

        redb_api.set("set", lua.create_async_function(|_, (key, value): (String, u64)| async move {
            write_db(key.as_str(), value).await.map_err(mlua::Error::external)?;
            Ok(())
        })?)?;

        redb_api.set("get", lua.create_async_function(|_, key: String| async move {
            read_db(key.as_str()).await.map_err(mlua::Error::external)?;
            Ok(())
        })?)?;
        
        lua.globals().set("redb_api", redb_api)?;
        Ok(())
    }
}