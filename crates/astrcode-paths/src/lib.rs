//! Process-level directory primitives shared across the workspace.
//!
//! These functions are the thinnest layer in the dependency graph: several
//! crates (including `astrcode-log`, which must stay lightweight) need the
//! data-directory layout without pulling in `astrcode-core`. The logical home
//! remains `astrcode_core::config::defaults`, which re-exports this crate.

use std::path::{Path, PathBuf};

const TEST_HOME_ENV: &str = "ASTRCODE_TEST_HOME";
const USER_HOME_ENV: &str = "ASTRCODE_HOME_DIR";

/// Returns the user home directory used by AstrCode.
///
/// The test-isolation directory takes precedence over the user override; when
/// neither is set, the system home directory is used, falling back to the
/// current directory when it cannot be resolved.
pub fn user_home_dir() -> PathBuf {
    env_path(TEST_HOME_ENV)
        .or_else(|| env_path(USER_HOME_ENV))
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Returns AstrCode's process-level data directory, `~/.astrcode` by default.
pub fn astrcode_dir() -> PathBuf {
    user_home_dir().join(".astrcode")
}

/// Returns the data directory for an extension below the given storage base:
/// `<base>/extension_data/<extension_id>`.
pub fn extension_data_dir(base: &Path, extension_id: &str) -> PathBuf {
    base.join("extension_data").join(extension_id)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::*;

    #[test]
    fn application_directories_follow_test_home() {
        let _guard = env_lock().lock().unwrap();
        let previous = std::env::var_os(TEST_HOME_ENV);
        // SAFETY: tests accessing this variable serialize through `env_lock`.
        unsafe { std::env::set_var(TEST_HOME_ENV, "/tmp/astrcode-paths-test") };

        assert_eq!(user_home_dir(), PathBuf::from("/tmp/astrcode-paths-test"));
        assert_eq!(
            astrcode_dir(),
            PathBuf::from("/tmp/astrcode-paths-test/.astrcode")
        );

        match previous {
            // SAFETY: tests accessing this variable serialize through `env_lock`.
            Some(value) => unsafe { std::env::set_var(TEST_HOME_ENV, value) },
            // SAFETY: tests accessing this variable serialize through `env_lock`.
            None => unsafe { std::env::remove_var(TEST_HOME_ENV) },
        }
    }

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }
}
