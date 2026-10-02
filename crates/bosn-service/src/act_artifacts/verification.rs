//! The private artifact cache and hash verification of cached files.

use super::*;

pub(super) fn private_cache(root: &Path) -> io::Result<u32> {
    if !root.is_absolute() {
        return Err(refused("artifact cache must be absolute"));
    }
    let user = ipc::current_user_id()?
        .parse::<u32>()
        .map_err(|_| refused("artifact cache user identity is invalid"))?;
    let mut current = PathBuf::new();
    let components = root.components().collect::<Vec<_>>();
    for (index, part) in components.iter().enumerate() {
        if !matches!(part, Component::RootDir | Component::Normal(_)) {
            return Err(refused("artifact cache path contains unsafe components"));
        }
        current.push(part.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                if metadata.uid() != 0 && metadata.uid() != user {
                    return Err(refused("artifact cache ancestor belongs to another user"));
                }
                // A sticky shared /tmp parent is allowed; replaceable parents
                // without that protection are not an ownership boundary.
                if metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0 {
                    return Err(refused(
                        "artifact cache ancestor is writable by another user",
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && index + 1 == components.len() => {
                ipc::ensure_owner_private_directory(&current)?;
            }
            _ => return Err(refused("artifact cache ancestor is not a real directory")),
        }
    }
    let metadata = std::fs::symlink_metadata(root)?;
    if metadata.uid().to_string() != ipc::current_user_id()? || metadata.mode() & 0o077 != 0 {
        return Err(refused("artifact cache is not owner private"));
    }
    Ok(metadata.uid())
}
pub(super) fn open_private(path: &Path, owner: u32) -> io::Result<std::fs::File> {
    // Linux UAPI O_NOFOLLOW and O_NONBLOCK: reject a link or FIFO without
    // following it or waiting for a writer, then validate the opened handle.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x20000 | 0x800)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(refused(
            "artifact cache entry is not a private regular file",
        ));
    }
    Ok(file)
}
pub(super) fn cache_lock(path: &Path, owner: u32) -> io::Result<fs::OwnedFileLock> {
    let file = match fs::create_private_file(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(0x20000 | 0x800)
            .open(path)?,
        Err(e) => return Err(e),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() != 0
    {
        return Err(refused(
            "artifact acquisition lock is not private and empty",
        ));
    }
    fs::try_lock_exclusive_owned(file)
}
pub(super) fn cache_usage(root: &Path, owner: u32) -> io::Result<u64> {
    let mut total = 0u64;
    let mut count = 0usize;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        count += 1;
        if count > 128 {
            return Err(refused("artifact cache entry ceiling exceeded"));
        }
        let name = entry.file_name();
        if name == ".acquire.lock" {
            continue;
        }
        // Crash leftovers are retained and stop acquisition; do not remove a
        // path whose current producer ownership cannot be established.
        if entry.file_type()?.is_dir() {
            return Err(refused(
                "artifact cache contains retained staging requiring recovery",
            ));
        }
        if name.to_str().is_none_or(|s| {
            s.len() != 64
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            return Err(refused("artifact cache contains an unexpected entry"));
        }
        let file = open_private(&entry.path(), owner)?;
        total = total
            .checked_add(file.metadata()?.len())
            .filter(|n| *n <= AGGREGATE)
            .ok_or_else(|| refused("artifact cache disk ceiling exceeded"))?;
    }
    Ok(total)
}
pub(super) fn verified_file(
    path: &Path,
    owner: u32,
    pin: &str,
    size: Option<u64>,
    ceiling: u64,
) -> io::Result<Vec<u8>> {
    let mut file = open_private(path, owner)?;
    let metadata = file.metadata()?;
    if metadata.len() > ceiling || size.is_some_and(|size| size != metadata.len()) {
        return Err(refused("cached artifact size mismatch"));
    }
    // Reserve the verified descriptor length exactly; do not let geometric
    // Vec growth double the advertised aggregate in-memory byte ceiling.
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| refused("cached artifact cannot fit memory"))?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut buffer = [0u8; 65536];
    let mut hasher = Sha256Hasher::new();
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if bytes.len().checked_add(count).is_none_or(|n| n > capacity) {
            return Err(refused("cached artifact grew beyond descriptor length"));
        }
        hasher.update(&buffer[..count]);
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() != capacity
        || size.is_some_and(|size| size != bytes.len() as u64)
        || format!("sha256:{}", hasher.finalize()) != pin
    {
        return Err(refused("cached artifact digest mismatch"));
    }
    Ok(bytes)
}
