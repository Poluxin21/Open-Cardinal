use std::{fmt::Error, path::{Path, PathBuf}};
use tokio::fs;


pub async fn find_rule_by_agent(agent_id: &String) -> Result<PathBuf, Error> {
    let safe_id = agent_id.replace("/", "").replace("\\", "").replace("..", "");
    let agent_dir = Path::new("rules").join(&safe_id);

    let target_dir = if fs::try_exists(&agent_dir).await.unwrap_or(false) {
        agent_dir
    } else {
        Path::new("rules").join("default")
    };

    Ok(target_dir)
}