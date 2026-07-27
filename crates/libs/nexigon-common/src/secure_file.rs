//! Atomic, no-follow file handling for credentials and private keys.

use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

/// Atomically replace `path` with a mode-0600 regular file.
///
/// When `private_parent` is true, the immediate parent is created or repaired to
/// mode 0700. Existing symlinks and non-regular destinations are rejected.
pub async fn write_private(
    path: &Path,
    contents: impl AsRef<[u8]>,
    private_parent: bool,
) -> io::Result<()> {
    write(path, contents.as_ref(), 0o600, private_parent).await
}

/// Atomically replace `path` with a mode-0644 regular file.
///
/// This is intended for the public certificate paired with a private key. Pass
/// `private_parent = true` when both identity files share a protected directory.
pub async fn write_public(
    path: &Path,
    contents: impl AsRef<[u8]>,
    private_parent: bool,
) -> io::Result<()> {
    write(path, contents.as_ref(), 0o644, private_parent).await
}

async fn write(path: &Path, contents: &[u8], mode: u32, private_parent: bool) -> io::Result<()> {
    let path = path.to_path_buf();
    let contents = contents.to_vec();
    tokio::task::spawn_blocking(move || {
        write_sync(&path, &contents, mode, private_parent, |_| Ok(()))
    })
    .await
    .map_err(|error| io::Error::other(format!("secure-file task failed: {error}")))?
}

/// Reject a path unless it is a non-symlink regular file with mode 0600.
pub async fn validate_private(path: &Path) -> io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || validate_private_sync(&path))
        .await
        .map_err(|error| io::Error::other(format!("secure-file task failed: {error}")))?
}

/// Reject a symlink or non-regular file without imposing a public/private mode.
pub async fn validate_regular(path: &Path) -> io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || validate_regular_sync(&path).map(|_| ()))
        .await
        .map_err(|error| io::Error::other(format!("secure-file task failed: {error}")))?
}

/// Read a non-symlink regular file without following its final path component.
pub async fn read_regular(path: &Path) -> io::Result<Vec<u8>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_sync(&path, false))
        .await
        .map_err(|error| io::Error::other(format!("secure-file task failed: {error}")))?
}

/// Read a non-symlink regular file and try to repair its mode to 0600 via the
/// opened file descriptor, avoiding a path-based check/read race. A failed
/// repair is warned about but does not make an existing installation unusable.
pub async fn read_private(path: &Path) -> io::Result<Vec<u8>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_sync(&path, true))
        .await
        .map_err(|error| io::Error::other(format!("secure-file task failed: {error}")))?
}

#[cfg(unix)]
fn read_sync(path: &Path, repair_private_mode: bool) -> io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;

    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if repair_private_mode
        && metadata.permissions().mode() & 0o777 != 0o600
        && let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600))
    {
        tracing::warn!(
            path = %path.display(),
            %error,
            "could not restrict private file permissions to mode 0600; continuing"
        );
    }
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    Ok(contents)
}

#[cfg(not(unix))]
fn read_sync(path: &Path, _repair_private_mode: bool) -> io::Result<Vec<u8>> {
    validate_regular_sync(path)?;
    fs::read(path)
}

#[cfg(unix)]
fn validate_regular_sync(path: &Path) -> io::Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a non-symlink regular file", path.display()),
        ));
    }
    Ok(metadata)
}

#[cfg(not(unix))]
fn validate_regular_sync(path: &Path) -> io::Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a non-symlink regular file", path.display()),
        ));
    }
    Ok(metadata)
}

#[cfg(unix)]
fn validate_private_sync(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = validate_regular_sync(path)?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} has mode {mode:04o}; expected 0600", path.display()),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_sync(path: &Path) -> io::Result<()> {
    validate_regular_sync(path).map(|_| ())
}

#[cfg(unix)]
fn prepare_parent(path: &Path, private_parent: bool) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "secure file has no parent"))?;
    fs::create_dir_all(parent)?;
    let metadata = fs::symlink_metadata(parent)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a non-symlink directory", parent.display()),
        ));
    }
    if private_parent && metadata.permissions().mode() & 0o777 != 0o700 {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    Ok(parent.to_path_buf())
}

#[cfg(not(unix))]
fn prepare_parent(path: &Path, _private_parent: bool) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "secure file has no parent"))?;
    fs::create_dir_all(parent)?;
    let metadata = fs::symlink_metadata(parent)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a non-symlink directory", parent.display()),
        ));
    }
    Ok(parent.to_path_buf())
}

struct TemporaryFile(Option<PathBuf>);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
fn write_sync<F>(
    path: &Path,
    contents: &[u8],
    mode: u32,
    private_parent: bool,
    before_rename: F,
) -> io::Result<()>
where
    F: FnOnce(&Path) -> io::Result<()>,
{
    use std::ffi::OsString;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    let parent = prepare_parent(path, private_parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a non-symlink regular file", path.display()),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "secure file has no filename")
    })?;
    let mut opened = None;
    for _ in 0..128 {
        let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = OsString::from(".");
        temporary_name.push(file_name);
        temporary_name.push(format!(".tmp.{}.{sequence}", std::process::id()));
        let temporary_path = parent.join(temporary_name);
        let result = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temporary_path);
        match result {
            Ok(file) => {
                opened = Some((temporary_path, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let (temporary_path, mut file) = opened.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "unable to allocate a unique secure-file temporary",
        )
    })?;
    let mut cleanup = TemporaryFile(Some(temporary_path.clone()));
    fs::set_permissions(&temporary_path, fs::Permissions::from_mode(mode))?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    before_rename(&temporary_path)?;
    fs::rename(&temporary_path, path)?;
    cleanup.0 = None;
    if let Err(error) = fs::File::open(&parent).and_then(|directory| directory.sync_all())
        && error.kind() != io::ErrorKind::InvalidInput
        && error.kind() != io::ErrorKind::Unsupported
    {
        return Err(error);
    }
    Ok(())
}

#[cfg(not(unix))]
fn write_sync<F>(
    path: &Path,
    contents: &[u8],
    _mode: u32,
    private_parent: bool,
    before_rename: F,
) -> io::Result<()>
where
    F: FnOnce(&Path) -> io::Result<()>,
{
    use std::io::Write;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    let parent = prepare_parent(path, private_parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a non-symlink regular file", path.display()),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let temporary_path = parent.join(format!(
        ".secure-file.tmp.{}.{sequence}",
        std::process::id()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)?;
    let mut cleanup = TemporaryFile(Some(temporary_path.clone()));
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    before_rename(&temporary_path)?;
    fs::rename(&temporary_path, path)?;
    cleanup.0 = None;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use tempfile::tempdir;

    use super::read_sync;
    use super::validate_private_sync;
    use super::write_sync;

    static UMASK_LOCK: Mutex<()> = Mutex::new(());

    struct UmaskGuard(libc::mode_t);

    impl Drop for UmaskGuard {
        fn drop(&mut self) {
            // SAFETY: restoring the process umask to the value captured by this test.
            unsafe { libc::umask(self.0) };
        }
    }

    #[test]
    fn private_write_is_atomic_and_private_under_common_umask() {
        let _lock = UMASK_LOCK.lock().unwrap();
        // SAFETY: serialized within this module and restored by `UmaskGuard`.
        let old_umask = unsafe { libc::umask(0o022) };
        let _guard = UmaskGuard(old_umask);
        let root = tempdir().unwrap();
        let parent = root.path().join("credentials");
        let path = parent.join("token.toml");

        write_sync(&path, b"secret", 0o600, true, |_| Ok(())).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"secret");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
        );
        assert_eq!(
            std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700,
        );
    }

    #[test]
    fn refuses_symlinks_and_repairs_existing_file_by_replacement() {
        let root = tempdir().unwrap();
        let victim = root.path().join("victim");
        let path = root.path().join("credentials");
        std::fs::write(&victim, b"do not replace").unwrap();
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        assert!(write_sync(&path, b"secret", 0o600, false, |_| Ok(())).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"do not replace");

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_sync(&path, b"new", 0o600, false, |_| Ok(())).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        validate_private_sync(&path).unwrap();
    }

    #[test]
    fn interrupted_write_keeps_destination_and_removes_temporary() {
        let root = tempdir().unwrap();
        let path = root.path().join("credentials");
        write_sync(&path, b"old", 0o600, false, |_| Ok(())).unwrap();

        let error = write_sync(&path, b"new", 0o600, false, |_| {
            Err(io::Error::other("injected interruption"))
        })
        .unwrap_err();

        assert_eq!(error.to_string(), "injected interruption");
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        let names = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, vec![path.file_name().unwrap()]);
    }

    #[test]
    fn private_validation_rejects_insecure_mode_and_symlink() {
        let root = tempdir().unwrap();
        let path = root.path().join("key.pem");
        std::fs::write(&path, b"key").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(validate_private_sync(&path).is_err());
        assert_eq!(read_sync(&path, true).unwrap(), b"key");
        validate_private_sync(&path).unwrap();

        let link = root.path().join("key-link.pem");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(validate_private_sync(&link).is_err());
        assert!(read_sync(&link, true).is_err());
    }
}
