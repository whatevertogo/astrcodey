use std::{io, os::unix::process::CommandExt, process::Command};

pub(crate) fn normalize_tmp_launch() -> io::Result<()> {
    let Some(executable) = astrcode_paths::trusted_tmp_launch_path(&std::env::current_exe()?)?
    else {
        return Ok(());
    };
    // Tauri caches its executable path before main; only a fresh process can
    // replace that cached /tmp rejection without disabling its symlink guard.
    Err(Command::new(executable)
        .args(std::env::args_os().skip(1))
        .exec())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn tmp_launch_reexecutes_with_tauri_path_validation_intact() {
        const CHILD: &str = "ASTRCODE_TMP_LAUNCH_TEST_CHILD";
        if let Some(marker) = std::env::var_os(CHILD) {
            let marker = Path::new(&marker);
            let argv0 = std::env::args_os().next().unwrap();
            if Path::new(&argv0).starts_with("/tmp") {
                assert!(!marker.exists(), "the alias launch must happen only once");
                std::fs::write(marker, "before exec").unwrap();
                assert!(tauri::process::current_binary(&tauri::Env::default()).is_err());
                normalize_tmp_launch().unwrap();
                panic!("normalization returned without replacing the process");
            }
            assert_eq!(std::fs::read_to_string(marker).unwrap(), "before exec");
            assert!(Path::new(&argv0).starts_with("/private/tmp"));
            assert!(std::env::current_exe().unwrap().starts_with("/private/tmp"));
            assert!(tauri::process::current_binary(&tauri::Env::default()).is_ok());
            std::fs::write(marker, "after exec").unwrap();
            return;
        }

        let temp = tempfile::tempdir_in("/private/tmp").unwrap();
        let executable = temp.path().join("desktop-test");
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let alias = Path::new("/tmp").join(executable.strip_prefix("/private/tmp").unwrap());
        let output = Command::new(alias)
            .args([
                "--exact",
                "startup::tests::tmp_launch_reexecutes_with_tauri_path_validation_intact",
                "--nocapture",
            ])
            .env(CHILD, temp.path().join("exec-marker"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("exec-marker")).unwrap(),
            "after exec"
        );
    }
}
