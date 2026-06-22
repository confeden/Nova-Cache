pub mod flush_thread;
mod ipc;
pub mod journal;
mod migration;
mod orchestrator;
mod perf_tracker;
pub mod l2_writer;
pub mod prefetch;
mod service;

use anyhow::Result;
use std::env;
use std::io::Write;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

fn set_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let _ = std::fs::write("panic.txt", format!("{}", info));
    }));
}

#[tokio::main]
async fn main() -> Result<()> {
    set_panic_hook();
    let log_dir = std::path::PathBuf::from("temp");
    let _ = std::fs::create_dir_all(&log_dir);

    let file_subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_dir.join("log.txt"))
                .expect("Failed to open log file"),
        ))
        .finish();
    tracing::subscriber::set_global_default(file_subscriber)
        .expect("setting default subscriber failed");

    let args: Vec<String> = env::args().collect();
    let console_mode = args.iter().any(|arg| arg == "--console");

    if console_mode {
        info!("Running Nova Cache Service in Console Mode...");
        match orchestrator::ServiceOrchestrator::new().await {
            Ok(mut orchestrator) => {
                info!("Orchestrator initialised. Press Ctrl+C to exit.");
                let is_test_run = args.iter().any(|arg| arg == "--test-run");
                let shutdown_flag = orchestrator.shutdown_flag();
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        info!("Ctrl+C received, shutting down gracefully...");
                    }
                    _ = async {
                        loop {
                            if shutdown_flag.load(std::sync::atomic::Ordering::Relaxed) {
                                info!("IPC shutdown requested, shutting down gracefully...");
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                    } => {}
                    _ = async {
                        if is_test_run {
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            info!("Test run complete. Auto-shutting down...");
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => {}
                }
                if let Err(e) = orchestrator.shutdown().await {
                    error!("Error during orchestrator shutdown: {:?}", e);
                }
            }
            Err(e) => {
                error!("Failed to initialise orchestrator: {:?}", e);
                std::process::exit(1);
            }
        }
    } else {
        // Run as Windows Service
        if let Err(e) = service::run_service() {
            error!("Windows Service stopped with error: {:?}", e);
            std::process::exit(1);
        }
    }

    Ok(())
}
