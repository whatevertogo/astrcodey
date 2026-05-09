use std::path::PathBuf;

use anyhow::Result;

pub fn astrcode_home() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("无法获取用户主目录"))?;
    Ok(home.join(".astrcode"))
}

pub fn instance_lock_path() -> Result<PathBuf> {
    Ok(astrcode_home()?.join("desktop.lock"))
}

pub fn instance_info_path() -> Result<PathBuf> {
    Ok(astrcode_home()?.join("desktop-instance.json"))
}
