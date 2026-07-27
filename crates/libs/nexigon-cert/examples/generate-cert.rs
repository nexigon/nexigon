use std::io;
use std::io::Write;
use std::path::Path;

use nexigon_cert::generate_self_signed_certificate;

pub fn main() {
    let args = std::env::args().skip(1).collect::<Vec<String>>();
    let [cert_path, key_path] = args.as_slice() else {
        eprintln!("usage: generate-cert <cert-path> <key-path>");
        std::process::exit(1);
    };
    let (certificate, key) = generate_self_signed_certificate();
    atomic_write(Path::new(key_path), key.as_bytes(), 0o600).unwrap();
    atomic_write(Path::new(cert_path), certificate.to_pem().as_bytes(), 0o644).unwrap();
}

fn atomic_write(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "output path has no parent"))?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    match std::fs::symlink_metadata(path) {
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
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}
