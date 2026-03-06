use std::path::Path;
use redb::{Database, Error, ReadableDatabase, TableDefinition};

use crate::utils::utils::load_config_file;
const FORCED_REACTIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("forced_reactions");

pub async fn add_queue(key: &str, value: &[u8]) -> Result<(), Error> {
    let queuefile = "queue.redb";
    let db: Database;

    if Path::new(&queuefile).exists() {
        db = Database::open(&queuefile)?;
    } else {
        db = Database::create(&queuefile)?;
    }

    let write_into = db.begin_write()?;
    {
        let mut table = write_into.open_table(FORCED_REACTIONS)?;
        table.insert(key, value)?;
    }
    write_into.commit()?;
    Ok(())
}

pub async fn read_queue(key: &str) -> Result<Option<Vec<u8>>, Error> {
    let queuefile = "queue.redb";

    let db = if Path::new(queuefile).exists() {
        Database::open(queuefile)?
    } else {
        Database::create(queuefile)?
    };

    let read_txn = db.begin_read()?;
    let table = read_txn.open_table(FORCED_REACTIONS)?;

    if let Some(value) = table.get(key)? {
        Ok(Some(value.value().to_vec()))
    } else {
        Ok(None)
    }
}

pub async fn remove_queue(key: &str) -> Result<(), Error> {
    let queuefile = "queue.redb";
    let db: Database;

    if Path::new(&queuefile).exists() {
        db = Database::open(&queuefile)?;
    } else {
        db = Database::create(&queuefile)?;
    }

    let remove_into = db.begin_write()?;

    {
        let mut table = remove_into.open_table(FORCED_REACTIONS)?;
        
        table.remove("agent123")?;
    }

    remove_into.commit()?;

    Ok(())
}