use std::{
    io,
    os::unix::{fs::MetadataExt, process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) fn normalize_tmp_launch() -> io::Result<()> {
    let Some(executable) = tmp_launch_path(&std::env::current_exe()?)? else {
        return Ok(());
    };
    // Tauri caches its executable path before main; only a fresh process can
    // replace that cached /tmp rejection without disabling its symlink guard.
    Err(Command::new(executable)
        .args(std::env::args_os().skip(1))
        .exec())
}

fn tmp_launch_path(executable: &Path) -> io::Result<Option<PathBuf>> {
    let Ok(relative) = executable.strip_prefix("/tmp") else {
        return Ok(None);
    };
    let alias = Path::new("/tmp");
    let metadata = alias.symlink_metadata()?;
    if !metadata.file_type().is_symlink()
        || metadata.uid() != 0
        || std::fs::read_link(alias)? != Path::new("private/tmp")
    {
        return Ok(None);
    }
    let resolved = Path::new("/private/tmp").join(relative);
    // Only the root-owned system alias is normalized. User-controlled links
    // elsewhere in the executable path must still be rejected by Tauri.
    for ancestor in resolved.ancestors() {
        if ancestor.symlink_metadata()?.file_type().is_symlink() {
            return Ok(None);
        }
    }
    Ok(Some(resolved))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn tmp_alias_is_normalized_without_accepting_other_symlinks() {
        let temp = tempfile::tempdir_in("/private/tmp").unwrap();
        let executable = temp.path().join("desktop");
        std::fs::write(&executable, "test").unwrap();
        let alias = Path::new("/tmp").join(executable.strip_prefix("/private/tmp").unwrap());
        assert_eq!(tmp_launch_path(&alias).unwrap(), Some(executable.clone()));
        assert_eq!(tmp_launch_path(&executable).unwrap(), None);
        assert_eq!(
            tmp_launch_path(Path::new(
                "/Applications/AstrCode.app/Contents/MacOS/astrcode-desktop"
            ))
            .unwrap(),
            None
        );

        let linked_file = temp.path().join("linked-desktop");
        symlink(&executable, &linked_file).unwrap();
        let linked_dir = temp.path().join("linked-dir");
        symlink(temp.path(), &linked_dir).unwrap();
        for path in [linked_file, linked_dir.join("desktop")] {
            let alias = Path::new("/tmp").join(path.strip_prefix("/private/tmp").unwrap());
            assert_eq!(tmp_launch_path(&alias).unwrap(), None);
        }
    }

    #[test]
    fn tmp_launch_reexecutes_with_tauri_path_validation_intact() {
        const CHILD: &str = "ASTRCODE_TMP_LAUNCH_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            if std::env::current_exe().unwrap().starts_with("/tmp") {
                assert!(tauri::process::current_binary(&tauri::Env::default()).is_err());
            }
            normalize_tmp_launch().unwrap();
            assert!(std::env::current_exe().unwrap().starts_with("/private/tmp"));
            assert!(tauri::process::current_binary(&tauri::Env::default()).is_ok());
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
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
