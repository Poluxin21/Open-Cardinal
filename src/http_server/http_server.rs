use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::RwLock;
use axum::{Router, extract::State, routing::get};
use tokio::fs;

#[derive(Clone)]
struct AppState {
    json_content: Arc<RwLock<String>>,
}

async fn watch_json_file(state: AppState) {
    let path = Path::new("info/sys.json");

    loop {
        if path.exists() {
            if let Ok(content) = fs::read_to_string(path).await {
                let mut data = state.json_content.write().await;
                if *data != content {
                    *data = content;
                }
            }
        }
        
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn get_metrics_handler(State(state): State<AppState>) -> String {
    state.json_content.read().await.clone()
}

pub async fn run_http_server() -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new("info/sys.json");
    
    while !path.exists() {
        println!("Waiting info sys...");
        tokio::time::sleep(Duration::from_secs(1)).await; 
    }

    let initial_content = fs::read_to_string(path).await?;

    let shared_state = AppState {
        json_content: Arc::new(RwLock::new(initial_content)),
    };

    tokio::spawn(watch_json_file(shared_state.clone()));

    let app = Router::new()
        .route("/metrics", get(get_metrics_handler))
        .with_state(shared_state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3001").await?;
    axum::serve(listener, app).await?;

    Ok(())
}