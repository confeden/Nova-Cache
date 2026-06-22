use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Command;
use tracing::{error, info};

/// Set the DSE (Driver Signature Enforcement) state flags via KDU.
///
/// Calls `kdu.exe -prv <provider_id> -dse <value>` to write
/// the specified value to the kernel's `g_CiOptions` variable.
pub fn set_dse_state(kdu_path: &Path, provider_id: u32, value: u32) -> Result<()> {
    if !kdu_path.exists() {
        return Err(anyhow!("KDU binary not found at {:?}", kdu_path));
    }

    info!(
        "Setting DSE state to {} via KDU provider {}",
        value, provider_id
    );

    let output = Command::new(kdu_path)
        .arg("-prv")
        .arg(provider_id.to_string())
        .arg("-dse")
        .arg(value.to_string())
        .output()
        .context("Failed to execute KDU process")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        error!("KDU -dse failed with status: {:?}", output.status);
        error!("KDU stdout:\n{}", stdout);
        error!("KDU stderr:\n{}", stderr);
        return Err(anyhow!(
            "KDU -dse failed with exit code {:?}. Stderr: {}",
            output.status.code(),
            stderr.trim()
        ));
    }

    info!("DSE state set to {} successfully.", value);
    if !stdout.is_empty() {
        info!("KDU output:\n{}", stdout.trim());
    }

    Ok(())
}

/// Temporarily disable Driver Signature Enforcement.
///
/// Sets the kernel CI options value to 0, allowing unsigned drivers
/// to be loaded through the standard Windows Service Control Manager.
pub fn disable_dse(kdu_path: &Path, provider_id: u32) -> Result<()> {
    info!("Disabling DSE temporarily...");
    set_dse_state(kdu_path, provider_id, 0)
}

/// Re-enable Driver Signature Enforcement.
///
/// Restores the kernel CI options value to 6
/// (the standard value for consumer Windows with DSE active).
pub fn enable_dse(kdu_path: &Path, provider_id: u32) -> Result<()> {
    info!("Re-enabling DSE...");
    set_dse_state(kdu_path, provider_id, 6)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_set_dse_fails_for_non_existent_kdu() {
        let kdu_path = PathBuf::from("C:\\non_existent_kdu_path.exe");
        let res = set_dse_state(&kdu_path, 11, 0);
        assert!(res.is_err());
        let err_msg = res.err().unwrap().to_string();
        assert!(err_msg.contains("KDU binary not found"));
    }
}
