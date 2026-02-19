use std::path::Path;
use redb::{Database, Error, ReadableDatabase, TableDefinition};

use crate::utils::utils::load_config_file;
const TABLE: TableDefinition<&str, u64> = TableDefinition::new("open_cardinal");

pub async fn write_db(key: &str, value: u64) -> Result<(), Error> {
    let config = load_config_file().await?;
    let db: Database;

    if Path::new(&config.db_file).exists() {
        db = Database::open(&config.db_file)?;
    } else {
        db = Database::create(&config.db_file)?;
    }

    let write_into = db.begin_write()?;
    {
        let mut table = write_into.open_table(TABLE)?;
        table.insert(key, value)?;
    }
    write_into.commit()?;
    Ok(())
}

pub async fn read_db(key: &str) -> Result<u64, Error> {
    let config = load_config_file().await?;
    let db: Database;

    if Path::new(&config.db_file).exists() {
        db = Database::open(&config.db_file)?;
    } else {
        db = Database::create(&config.db_file)?;
    }

    let read_into = db.begin_read()?;
    let table = read_into.open_table(TABLE)?;
    Ok(table.get(key)?.unwrap().value())
}