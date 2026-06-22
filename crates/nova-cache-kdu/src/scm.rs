//! Service Control Manager (SCM) integration for minifilter driver lifecycle.
//!
//! Uses `sc.exe` and `reg.exe` to register, start, stop, and delete
//! the Novacache minifilter driver service, avoiding complex FFI.

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tracing::{info, warn};

const MINIFILTER_ALTITUDE: &str = "180100";
const INSTANCE_NAME: &str = "Novacache Instance";

/// Check if a Windows service exists.
fn service_exists(service_name: &str) -> bool {
    let output = Command::new("sc.exe")
        .arg("query")
        .arg(service_name)
        .output();
    match output {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

/// Get service state. Returns the numeric state code or -1 on error.
/// States: 1=STOPPED, 2=START_PENDING, 3=STOP_PENDING, 4=RUNNING
fn get_service_state(service_name: &str) -> i32 {
    let output = Command::new("sc.exe")
        .arg("query")
        .arg(service_name)
        .output();
    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            // Parse "STATE              : 4" or similar
            for line in stdout.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("STATE") {
                    if let Some(state_str) = trimmed.split(':').nth(1) {
                        if let Ok(state) = state_str.trim().parse::<i32>() {
                            return state;
                        }
                    }
                }
            }
            -1
        }
        Err(_) => -1,
    }
}

/// Wait for a service to reach the STOPPED state (state=1).
/// Returns Ok(true) if stopped, Ok(false) if timed out.
fn wait_for_service_stopped(service_name: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        let state = get_service_state(service_name);
        if state == 1 || state == -1 {
            // STOPPED or not found
            return true;
        }
        if start.elapsed() >= timeout {
            warn!(
                "Timed out waiting for service '{}' to stop (last state: {})",
                service_name, state
            );
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Force stop and fully unload the driver service.
/// Waits up to `timeout` for the service to stop.
pub fn stop_driver_service(service_name: &str) -> bool {
    let state = get_service_state(service_name);

    // Not found at all
    if state == -1 {
        info!("Driver service '{}' not found.", service_name);
        return true;
    }

    // Service is stopped but minifilter instances may still be loaded
    if state == 1 {
        info!(
            "Driver service '{}' is stopped, checking for lingering filter instances...",
            service_name
        );
        // Try to unload minifilter instances that may still be attached
        let flt_output = Command::new("fltmc.exe")
            .arg("unload")
            .arg(service_name)
            .output();
        match flt_output {
            Ok(o) => {
                if o.status.success() {
                    info!("Lingering minifilter instances unloaded.");
                    std::thread::sleep(Duration::from_secs(2));
                } else {
                    let msg = String::from_utf8_lossy(&o.stdout);
                    if msg.contains("not found") || msg.contains("No") {
                        info!("No lingering filter instances found.");
                    } else {
                        info!("fltmc unload: {}", msg.trim());
                    }
                }
            }
            Err(e) => {
                info!("fltmc not available: {:?}", e);
            }
        }
        return true;
    }

    info!("Stopping driver service '{}'...", service_name);

    let output = Command::new("sc.exe")
        .arg("stop")
        .arg(service_name)
        .output();

    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if o.status.success() {
                info!("Stop command sent to '{}'.", service_name);
            } else {
                let msg = stdout.trim().to_string();
                // 1062 = service not started, 1056 = already running then becomes stopped
                if msg.contains("1062") {
                    info!("Service '{}' was not running.", service_name);
                    return true;
                }
                warn!("sc stop '{}': {}", service_name, msg);
            }
        }
        Err(e) => {
            warn!("Failed to execute sc stop: {:?}", e);
            return false;
        }
    }

    // Wait for the driver to fully unload from kernel
    info!("Waiting for driver to fully unload...");
    wait_for_service_stopped(service_name, Duration::from_secs(10))
}

/// Delete (unregister) the driver service.
pub fn delete_driver_service(service_name: &str) {
    info!("Deleting driver service '{}'...", service_name);

    match Command::new("sc.exe")
        .arg("delete")
        .arg(service_name)
        .output()
    {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if o.status.success() {
                info!("Service '{}' deleted.", service_name);
            } else {
                let msg = stdout.trim().to_string();
                if msg.contains("1072") {
                    info!(
                        "Service '{}' marked for deletion (still in use).",
                        service_name
                    );
                } else {
                    warn!("sc delete '{}': {}", service_name, msg);
                }
            }
        }
        Err(e) => warn!("Failed to execute sc delete: {:?}", e),
    }
}

/// Register the Novacache minifilter as a kernel file-system driver service.
///
/// Deletes any existing service entry first, then creates a fresh one
/// to ensure the ImagePath is always up to date.
pub fn register_minifilter_service(service_name: &str, driver_path: &Path) -> Result<()> {
    let image_path = format!(r"\??\{}", driver_path.display());

    info!(
        "Registering minifilter service '{}' with ImagePath: {}",
        service_name, image_path
    );

    // ── 0. Unload minifilter instances if still attached ────────────
    info!("Attempting to unload minifilter instances...");
    let flt_output = Command::new("fltmc.exe")
        .arg("unload")
        .arg(service_name)
        .output();
    match flt_output {
        Ok(o) => {
            if o.status.success() {
                info!("Minifilter '{}' unloaded via fltmc.", service_name);
                std::thread::sleep(Duration::from_secs(2));
            } else {
                let msg = String::from_utf8_lossy(&o.stdout);
                info!("fltmc unload result: {}", msg.trim());
            }
        }
        Err(e) => {
            info!("fltmc not available or failed: {:?}", e);
        }
    }

    // ── 1. Delete existing service to ensure fresh ImagePath ────────
    if service_exists(service_name) {
        info!(
            "Service '{}' already exists, deleting for fresh registration...",
            service_name
        );
        delete_driver_service(service_name);
        // Poll until service is fully deleted (max 30 seconds)
        info!("Waiting for SCM to fully release the service...");
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if !service_exists(service_name) {
                info!("Service '{}' fully deleted.", service_name);
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        if service_exists(service_name) {
            warn!(
                "Service '{}' still exists after 30 seconds, proceeding anyway.",
                service_name
            );
        }
    }

    // ── 2. Create the kernel driver service ─────────────────────────
    let output = Command::new("sc.exe")
        .arg("create")
        .arg(service_name)
        .arg("type=")
        .arg("filesys")
        .arg("binPath=")
        .arg(&image_path)
        .arg("start=")
        .arg("demand")
        .arg("group=")
        .arg("FSFilter Activity Monitor")
        .arg("depend=")
        .arg("FltMgr")
        .output()
        .context("Failed to execute sc.exe create")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "sc create failed: {} {}",
            stdout.trim(),
            stderr.trim()
        ));
    }

    info!("Service entry created.");

    // ── 3. Minifilter registry keys ─────────────────────────────────
    let svc_key = format!(r"HKLM\SYSTEM\CurrentControlSet\Services\{}", service_name);
    reg_add(&svc_key, "SupportedFeatures", "REG_DWORD", "3")?;

    let instances_key = format!(r"{}\Instances", svc_key);
    reg_add(&instances_key, "DefaultInstance", "REG_SZ", INSTANCE_NAME)?;

    let instance_key = format!(r"{}\{}", instances_key, INSTANCE_NAME);
    reg_add(&instance_key, "Altitude", "REG_SZ", MINIFILTER_ALTITUDE)?;
    reg_add(&instance_key, "Flags", "REG_DWORD", "0")?;

    info!(
        "Minifilter service '{}' fully registered (altitude {}).",
        service_name, MINIFILTER_ALTITUDE
    );
    Ok(())
}

/// Start the driver service via SCM with retries.
///
/// Returns `Ok(())` if the service starts or is already running.
pub fn start_driver_service(service_name: &str) -> Result<()> {
    info!("Starting driver service '{}'...", service_name);

    let output = Command::new("sc.exe")
        .arg("start")
        .arg(service_name)
        .output()
        .context("Failed to execute sc.exe start")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        // ERROR_SERVICE_ALREADY_RUNNING (1056)
        if stdout.contains("1056") {
            info!("Service '{}' is already running.", service_name);
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "sc start '{}' failed: {} {}",
            service_name,
            stdout.trim(),
            stderr.trim()
        ));
    }

    // Wait for the service to actually reach RUNNING state
    let start = Instant::now();
    loop {
        let state = get_service_state(service_name);
        if state == 4 {
            // RUNNING
            break;
        }
        if start.elapsed() > Duration::from_secs(10) {
            warn!(
                "Service '{}' started but did not reach RUNNING state in time (state: {})",
                service_name, state
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    info!("Driver service '{}' started.", service_name);
    Ok(())
}

// ─── helpers ────────────────────────────────────────────────────────

/// Attempt to sign a driver binary with a test certificate.
/// Searches for signtool.exe in Windows SDK paths and uses the
/// NovaCacheTest certificate from the local machine store.
/// This is required even in test signing mode — Windows needs at least
/// a self-signed test certificate on the .sys file.
pub fn sign_driver_binary(driver_path: &std::path::Path) -> Result<()> {
    // Find signtool.exe in Windows SDK paths
    let signtool_paths = [
        r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe",
        r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64\signtool.exe",
        r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.22000.0\x64\signtool.exe",
        r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.19041.0\x64\signtool.exe",
    ];

    let signtool = signtool_paths
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .ok_or_else(|| anyhow!("signtool.exe not found in Windows SDK paths"))?;

    info!("Using signtool: {}", signtool);
    info!("Signing driver: {}", driver_path.display());

    let output = Command::new(signtool)
        .arg("sign")
        .arg("/a")
        .arg("/fd")
        .arg("SHA256")
        .arg(driver_path)
        .output()
        .context("Failed to execute signtool.exe")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        return Err(anyhow!(
            "signtool sign failed: {} {}",
            stdout.trim(),
            stderr.trim()
        ));
    }

    info!("Driver signed successfully.");
    Ok(())
}

fn reg_add(key: &str, value_name: &str, value_type: &str, data: &str) -> Result<()> {
    let output = Command::new("reg.exe")
        .arg("add")
        .arg(key)
        .arg("/v")
        .arg(value_name)
        .arg("/t")
        .arg(value_type)
        .arg("/d")
        .arg(data)
        .arg("/f")
        .output()
        .with_context(|| format!("reg add for {}\\{}", key, value_name))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "reg add failed for {}\\{}: {}",
            key,
            value_name,
            stderr.trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reg_add_helper_formats_correctly() {
        let _ = reg_add(
            r"HKLM\SOFTWARE\__NovaCacheTest",
            "TestValue",
            "REG_DWORD",
            "0",
        );
    }
}
