use std::{fs::File, io::{BufReader, Error}, path::Path};

use tokio::fs;
use tracing::info;

use crate::kernel::models::sys_json::ConfigJson;

const CONFIG_FILE: &str = "config/config.json";

pub async fn load_config_file() -> Result<ConfigJson, Error> {
    if !Path::new(CONFIG_FILE).exists() {
        info!("Config file founded");
    }

    let file = File::open(CONFIG_FILE)?;
    let reader = BufReader::new(file);

    let config: ConfigJson = serde_json::from_reader(reader)?;

    Ok(config)
}