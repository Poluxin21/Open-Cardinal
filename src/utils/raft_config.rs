use std::path::Path;
use tokio::fs;

pub async fn create_(mut path: String, c) {
    fs::create_dir(path.clone()).await;
    fs::File::create(path.clone()).await;
}