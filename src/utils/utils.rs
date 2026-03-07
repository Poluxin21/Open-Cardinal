use std::{fs::File, io::{BufReader, Error}, path::Path};
use tracing::info;
use redb::{Database, TableDefinition};

use crate::kernel::models::sys_json::ConfigJson;

const CONFIG_FILE: &str = "config/config.json";
const FORCED_REACTIONS: TableDefinition<&str, i32> = TableDefinition::new("forced_reactions");
const OPEN_CARDINAL: TableDefinition<&str, u64> = TableDefinition::new("open_cardinal");

pub async fn load_config_file() -> Result<ConfigJson, Error> {
    if !Path::new(CONFIG_FILE).exists() {
        info!("Config file founded");
    }

    let file = File::open(CONFIG_FILE)?;
    let reader = BufReader::new(file);

    let config: ConfigJson = serde_json::from_reader(reader)?;

    Ok(config)
}

pub fn init_databases() -> Result<(), redb::Error> {
    // Queue DB
    let queue_db = if Path::new("queue.redb").exists() {
        Database::open("queue.redb")?
    } else {
        Database::create("queue.redb")?
    };

    {
        let write_txn = queue_db.begin_write()?;
        write_txn.open_table(FORCED_REACTIONS)?;
        write_txn.commit()?;
    }

    let cardinal_db = if Path::new("open_cardinal.redb").exists() {
        Database::open("open_cardinal.redb")?
    } else {
        Database::create("open_cardinal.redb")?
    };

    {
        let write_txn = cardinal_db.begin_write()?;
        write_txn.open_table(OPEN_CARDINAL)?;
        write_txn.commit()?;
    }

    Ok(())
}