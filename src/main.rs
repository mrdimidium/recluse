// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use axum::Router;
use std::path::PathBuf;
use std::sync::Arc;

mod controller_web;
mod controller_zig;
mod service_config;
mod service_storage;
mod service_upstream;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HELP: &str = "\
Usage: zorian [--config=<path>]

Options:
  --config=<path>    Path to config file (optional)
  --help             Show this help message
  --version          Show version
";

#[tokio::main]
async fn main() {
    let mut config_path = None;
    for arg in std::env::args().skip(1) {
        if arg == "--help" || arg == "-h" {
            print!("{HELP}");
            return;
        }
        if arg == "--version" || arg == "-V" {
            println!("zorian {VERSION}");
            return;
        }
        if let Some(path) = arg.strip_prefix("--config=") {
            config_path = Some(PathBuf::from(path));
        }
    }

    let config = Arc::new(match config_path {
        Some(path) => service_config::ConfigService::from_file(&path).unwrap_or_else(|e| {
            eprintln!("error: {e}");
            std::process::exit(1);
        }),
        None => service_config::ConfigService::default(),
    });
    config.validate().unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

    let storage = Arc::new(service_storage::StorageService::new(config.clone()));
    let upstream = Arc::new(service_upstream::UpstreamService::new());

    let web_controller = Arc::new(controller_web::WebController::new());
    let zig_controller = Arc::new(controller_zig::ZigController::new(
        config.clone(),
        storage.clone(),
        upstream.clone(),
    ));

    let app = Router::new()
        .merge(web_controller.router())
        .merge(zig_controller.router());

    let listener = tokio::net::TcpListener::bind(config.listen())
        .await
        .unwrap();

    println!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}
