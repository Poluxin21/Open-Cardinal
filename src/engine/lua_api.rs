use crate::engine::storage::{read_db, write_db};
#[allow(non_snake_case)]
use crate::g_rpc::g_rpc::cardinal_core::Reaction;
use mlua::prelude::*;
use std::collections::HashMap;

use super::models::lua_input::LuaInput;
use super::models::lua_output::LuaOutput;

pub async fn run_script(
    lua: &Lua,
    script_content: &str,
    input: &LuaInput<'_>,
) -> LuaResult<LuaOutput> {
    let input_value = lua.to_value(input)?;

    let globals = lua.globals();
    globals.set("pulse", input_value)?;

    let value: mlua::Value = lua.load(script_content).eval_async().await?;

    if let mlua::Value::Nil = value {
        return Err(mlua::Error::RuntimeError("No action".into()));
    }

    let output: LuaOutput = lua.from_value(value)?;

    Ok(output)
}

pub fn convert_to_proto(out: LuaOutput) -> Reaction {
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

pub fn make_idle(msg: &str) -> Reaction {
    Reaction {
        trace_id: "idle".to_string(),
        r#type: 0,
        command_name: msg.to_string(),
        parameters: HashMap::new(),
    }
}

pub async fn inject_redb_api(lua: &Lua) -> LuaResult<()> {
    let redb_api = lua.create_table()?;

    redb_api.set(
        "set",
        lua.create_async_function(|_, (key, value): (String, u64)| async move {
            write_db(key.as_str(), value)
                .await
                .map_err(mlua::Error::external)?;
            Ok(())
        })?,
    )?;

    redb_api.set(
        "get",
        lua.create_async_function(|_, key: String| async move {
            let value = read_db(key.as_str()).await.map_err(mlua::Error::external)?;
            Ok(value)
        })?,
    )?;

    lua.globals().set("redb_api", redb_api)?;
    Ok(())
}
