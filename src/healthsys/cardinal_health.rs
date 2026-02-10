use sysinfo::System;
use tokio::time::{sleep, Duration};

use crate::healthsys::health::monitor;

pub async fn run(mut sys: System) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let payload = monitor::collect(&mut sys)?;
        monitor::persist(&payload).await?;

        sleep(Duration::from_secs(1)).await;
    }
}
