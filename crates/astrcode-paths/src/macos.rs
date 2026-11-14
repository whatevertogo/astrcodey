use std::{
    fs::{File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

// Darwin's <sys/acl.h>; libc does not currently expose these ACL functions.
const ACL_TYPE_EXTENDED: libc::c_int = 0x100;
unsafe extern "C" {
    fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> *mut libc::c_void;
    fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
}

/// Resolves macOS's root-owned /tmp alias only when another user cannot replace
/// or modify the executable or its ancestors. Other aliases are not normalized.
/// Extended ACLs are conservatively rejected because mode bits alone cannot
/// establish their write permissions. Inspection errors fail closed.
pub fn trusted_tmp_launch_path(executable: &Path) -> io::Result<Option<PathBuf>> {
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
    // SAFETY: geteuid has no arguments or memory preconditions.
    let uid = unsafe { libc::geteuid() };
    // Validate from the root down: each checked parent protects the next entry.
    // /private/tmp is root-owned and sticky, so it protects entries owned by us.
    for ancestor in resolved.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(ancestor)?;
        let metadata = file.metadata()?;
        let system_tmp = ancestor == Path::new("/private/tmp")
            && metadata.uid() == 0
            && metadata.mode() & 0o1000 != 0;
        if (metadata.uid() != 0 && metadata.uid() != uid)
            || (metadata.mode() & 0o022 != 0 && !system_tmp)
            || (ancestor == resolved && !metadata.is_file())
            || (ancestor != resolved && !metadata.is_dir())
            || has_extended_acl(&file)?
        {
            return Ok(None);
        }
    }
    Ok(Some(resolved))
}

fn has_extended_acl(file: &File) -> io::Result<bool> {
    // SAFETY: the descriptor is live and ACL_TYPE_EXTENDED is Darwin's ACL type.
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        // Darwin returns ENOENT for an absent ACL on an already-open descriptor.
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(false)
        } else {
            Err(error)
        };
    }
    // SAFETY: acl was allocated by acl_get_fd_np and has not been freed.
    unsafe { acl_free(acl) };
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        process::Command,
    };

    use super::*;

    #[test]
    fn tmp_launch_rejects_links_shared_writes_and_extended_acls() {
        let temp = tempfile::tempdir_in("/private/tmp").unwrap();
        let executable = temp.path().join("desktop");
        std::fs::write(&executable, "test").unwrap();
        let alias = Path::new("/tmp").join(executable.strip_prefix("/private/tmp").unwrap());
        assert_eq!(
            trusted_tmp_launch_path(&alias).unwrap(),
            Some(executable.clone())
        );
        assert_eq!(trusted_tmp_launch_path(&executable).unwrap(), None);
        assert_eq!(
            trusted_tmp_launch_path(Path::new(
                "/Applications/AstrCode.app/Contents/MacOS/astrcode-desktop"
            ))
            .unwrap(),
            None
        );

        for path in [&executable, temp.path()] {
            let mode = path.metadata().unwrap().permissions().mode();
            for extra in [0o020, 0o002] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | extra))
                    .unwrap();
                assert_eq!(trusted_tmp_launch_path(&alias).unwrap(), None);
            }
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(trusted_tmp_launch_path(&alias).unwrap().is_some());
            assert!(
                Command::new("/bin/chmod")
                    .args(["+a", "everyone allow write"])
                    .arg(path)
                    .status()
                    .unwrap()
                    .success()
            );
            assert_eq!(trusted_tmp_launch_path(&alias).unwrap(), None);
            assert!(
                Command::new("/bin/chmod")
                    .arg("-N")
                    .arg(path)
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(trusted_tmp_launch_path(&alias).unwrap().is_some());
        }

        let linked_file = temp.path().join("linked-desktop");
        symlink(&executable, &linked_file).unwrap();
        let linked_dir = temp.path().join("linked-dir");
        symlink(temp.path(), &linked_dir).unwrap();
        for path in [linked_file, linked_dir.join("desktop")] {
            let alias = Path::new("/tmp").join(path.strip_prefix("/private/tmp").unwrap());
            assert!(trusted_tmp_launch_path(&alias).is_err());
        }
    }
}
