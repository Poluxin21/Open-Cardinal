use mlua::Lua;
use tracing::{info, error};

pub async fn lua_check(script_content: &str, file_name: &str) -> Result<(), mlua::Error> {
    let lua = Lua::new();

    match lua
        .load(script_content)
        .set_name(file_name)
        .exec()
    {
        Ok(_) => {
            info!("Load Script with success {}", file_name);
            Ok(())
        }
        Err(e) => {
            error!("Error in script {}, error: {}", file_name, e);
            Err(e)
        }
    }
}