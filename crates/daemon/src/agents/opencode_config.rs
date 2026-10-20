//! Refresh regular OpenCode config files without re-importing logged-out auth.
use std::{
    collections::HashSet,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
const CONFIGS: [&str; 3] = ["config.json", "opencode.json", "opencode.jsonc"];
pub(crate) const CONFIG_READ_LIMIT: usize = 8 * 1024 * 1024;
const MANIFEST: &str = ".native-config-mirrors.json";
pub const USER_CONFIG_ROOT_ENV: &str = "BS_OPENCODE_CONFIG_ROOT";

/// The desktop's Windows-user source wins over the daemon user's XDG source.
/// Empty and relative values do not qualify as XDG base directories.
pub fn native_config_root(
    host: &Path,
    configured: Option<&std::ffi::OsStr>,
    xdg: Option<&std::ffi::OsStr>,
) -> PathBuf {
    configured
        .filter(|path| Path::new(path).is_absolute())
        .or_else(|| xdg.filter(|path| Path::new(path).is_absolute()))
        .map(PathBuf::from)
        .unwrap_or_else(|| host.join(".config"))
}

fn context(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("OpenCode configuration {}: {error}", path.display()),
    )
}
fn refuse(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied,format!("OpenCode configuration {} must be an ordinary daemon-owned directory or file, not a link",path.display()))
}

/// Unix uses directory descriptors and NOFOLLOW so an agent replacing a
/// writable parent cannot redirect a mirror write into a host directory.
struct ManagedDir {
    path: PathBuf,
    root: PathBuf,
    #[cfg(unix)]
    fd: std::fs::File,
}
impl ManagedDir {
    fn root(root: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(root).map_err(|e| context(root, e))?;
        let path = std::fs::canonicalize(root).map_err(|e| context(root, e))?;
        #[cfg(unix)]
        let fd = std::fs::File::open(&path).map_err(|e| context(&path, e))?;
        Ok(Self {
            root: path.clone(),
            path,
            #[cfg(unix)]
            fd,
        })
    }
    fn child(&self, name: &str) -> io::Result<Self> {
        let path = self.path.join(name);
        #[cfg(unix)]
        {
            use nix::{
                fcntl::{openat, OFlag},
                sys::stat::{mkdirat, Mode},
            };
            use std::os::fd::{AsRawFd, FromRawFd};
            let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
            let raw = match openat(Some(self.fd.as_raw_fd()), name, flags, Mode::empty()) {
                Ok(fd) => fd,
                Err(nix::errno::Errno::ENOENT) => {
                    mkdirat(Some(self.fd.as_raw_fd()), name, Mode::S_IRWXU)
                        .map_err(|e| context(&path, io::Error::from_raw_os_error(e as i32)))?;
                    openat(Some(self.fd.as_raw_fd()), name, flags, Mode::empty())
                        .map_err(|_| refuse(&path))?
                }
                Err(_) => return Err(refuse(&path)),
            };
            let fd = unsafe { std::fs::File::from_raw_fd(raw) };
            Ok(Self {
                path,
                root: self.root.clone(),
                fd,
            })
        }
        #[cfg(not(unix))]
        {
            self.check_parent()?;
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    if linked(&metadata) || !metadata.is_dir() {
                        return Err(refuse(&path));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    std::fs::create_dir(&path).map_err(|e| context(&path, e))?
                }
                Err(e) => return Err(context(&path, e)),
            }
            let result = Self {
                path,
                root: self.root.clone(),
            };
            result.check_parent()?;
            Ok(result)
        }
    }
    #[cfg(not(unix))]
    fn check_parent(&self) -> io::Result<()> {
        let mut path = self.root.clone();
        for component in self
            .path
            .strip_prefix(&self.root)
            .map_err(|_| refuse(&self.path))?
            .components()
        {
            path.push(component);
            let metadata = std::fs::symlink_metadata(&path).map_err(|e| context(&path, e))?;
            if linked(&metadata) || !metadata.is_dir() {
                return Err(refuse(&path));
            }
        }
        Ok(())
    }
    fn exists(&self, name: &str) -> io::Result<bool> {
        let path = self.path.join(name);
        #[cfg(unix)]
        {
            use nix::{
                fcntl::AtFlags,
                sys::stat::{fstatat, SFlag},
            };
            use std::os::fd::AsRawFd;
            match fstatat(
                Some(self.fd.as_raw_fd()),
                name,
                AtFlags::AT_SYMLINK_NOFOLLOW,
            ) {
                Ok(stat) => {
                    if SFlag::from_bits_truncate(stat.st_mode) == SFlag::S_IFREG {
                        Ok(true)
                    } else {
                        Err(refuse(&path))
                    }
                }
                Err(nix::errno::Errno::ENOENT) => Ok(false),
                Err(e) => Err(context(&path, io::Error::from_raw_os_error(e as i32))),
            }
        }
        #[cfg(not(unix))]
        {
            self.check_parent()?;
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    if linked(&metadata) || !metadata.is_file() {
                        Err(refuse(&path))
                    } else {
                        Ok(true)
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(context(&path, e)),
            }
        }
    }
    fn read(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        if !self.exists(name)? {
            return Ok(None);
        }
        #[cfg(unix)]
        let file = {
            use nix::fcntl::{openat, OFlag};
            use nix::sys::stat::Mode;
            use std::os::fd::{AsRawFd, FromRawFd};
            let fd = openat(
                Some(self.fd.as_raw_fd()),
                name,
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
                Mode::empty(),
            )
            .map_err(|e| {
                context(
                    &self.path.join(name),
                    io::Error::from_raw_os_error(e as i32),
                )
            })?;
            unsafe { std::fs::File::from_raw_fd(fd) }
        };
        #[cfg(not(unix))]
        let file = std::fs::File::open(self.path.join(name))
            .map_err(|e| context(&self.path.join(name), e))?;
        if !file
            .metadata()
            .map_err(|e| context(&self.path.join(name), e))?
            .is_file()
        {
            return Err(refuse(&self.path.join(name)));
        }
        let mut bytes = vec![];
        file.take(4096)
            .read_to_end(&mut bytes)
            .map_err(|e| context(&self.path.join(name), e))?;
        Ok(Some(bytes))
    }
    fn write(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.exists(name)?;
        let temporary = crate::ids::new_id(".mirror_");
        #[cfg(unix)]
        {
            use nix::{
                fcntl::{openat, renameat, OFlag},
                sys::stat::Mode,
                unistd::{unlinkat, UnlinkatFlags},
            };
            use std::os::fd::{AsRawFd, FromRawFd};
            let parent = Some(self.fd.as_raw_fd());
            let fd = openat(
                parent,
                temporary.as_str(),
                OFlag::O_WRONLY
                    | OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_CLOEXEC,
                Mode::S_IRUSR | Mode::S_IWUSR,
            )
            .map_err(|e| {
                context(
                    &self.path.join(name),
                    io::Error::from_raw_os_error(e as i32),
                )
            })?;
            let result = (|| {
                let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
                file.write_all(bytes)?;
                file.sync_all()?;
                renameat(parent, temporary.as_str(), parent, name)
                    .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
                Ok(())
            })();
            if result.is_err() {
                let _ = unlinkat(parent, temporary.as_str(), UnlinkatFlags::NoRemoveDir);
            }
            result.map_err(|e| context(&self.path.join(name), e))
        }
        #[cfg(not(unix))]
        {
            self.check_parent()?;
            let tmp = self.path.join(&temporary);
            let result = (|| {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)?;
                file.write_all(bytes)?;
                file.sync_all()?;
                drop(file);
                self.check_parent()?;
                self.exists(name)?;
                std::fs::rename(&tmp, self.path.join(name))
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            result.map_err(|e| context(&self.path.join(name), e))
        }
    }
    fn remove(&self, name: &str) -> io::Result<()> {
        if !self.exists(name)? {
            return Ok(());
        }
        #[cfg(unix)]
        {
            use nix::unistd::{unlinkat, UnlinkatFlags};
            use std::os::fd::AsRawFd;
            unlinkat(Some(self.fd.as_raw_fd()), name, UnlinkatFlags::NoRemoveDir).map_err(|e| {
                context(
                    &self.path.join(name),
                    io::Error::from_raw_os_error(e as i32),
                )
            })
        }
        #[cfg(not(unix))]
        {
            self.check_parent()?;
            std::fs::remove_file(self.path.join(name))
                .map_err(|e| context(&self.path.join(name), e))
        }
    }
}
#[cfg(not(unix))]
fn linked(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Nonblocking ordinary-file reads. Agent-owned snapshots cannot follow a
/// replacement link; trusted regular config sources may use dotfile symlinks.
pub(crate) fn read_regular_file(
    path: &Path,
    limit: usize,
    follow_links: bool,
) -> io::Result<Vec<u8>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(
            nix::libc::O_NONBLOCK
                | if follow_links {
                    0
                } else {
                    nix::libc::O_NOFOLLOW
                },
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        if !follow_links {
            options.custom_flags(0x00200000);
        } // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    let is_link = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let is_link = metadata.file_type().is_symlink();
    if !metadata.is_file() || (!follow_links && is_link) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an ordinary configuration file",
        ));
    }
    let mut bytes = vec![];
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("configuration file exceeds the {limit}-byte read limit"),
        ));
    }
    Ok(bytes)
}

fn source(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match read_regular_file(path, CONFIG_READ_LIMIT, true) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(context(path, e)),
    }
}

pub fn seed_native_files(root: &Path, config: &Path, data: &Path) -> io::Result<()> {
    static IMPORT: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    let _guard = IMPORT.lock();
    let home = ManagedDir::root(root)?.child("opencode-home")?;
    let already_imported = home.exists(".native-imported")?;
    let manifest = home.read(MANIFEST)?;
    let mut mirrored = match &manifest {
        Some(bytes) => serde_json::from_slice::<HashSet<String>>(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "OpenCode configuration {} has an invalid mirror manifest",
                    home.path.join(MANIFEST).display()
                ),
            )
        })?,
        None => HashSet::new(),
    };
    if mirrored
        .iter()
        .any(|name| !CONFIGS.contains(&name.as_str()))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "OpenCode config mirror manifest contains an unsupported filename",
        ));
    }
    let native = config.join("opencode");
    // Read every source before changing destinations; a failed read must not
    // leave half the configuration updated or silently retain a stale file.
    let sources = CONFIGS
        .iter()
        .map(|name| source(&native.join(name)).map(|bytes| (*name, bytes)))
        .collect::<io::Result<Vec<_>>>()?;
    let target = home.child("config")?.child("opencode")?;
    // A caller can already use the managed path as its regular config root.
    // Do not replace native files with themselves or claim deletion ownership.
    let same_config = std::fs::canonicalize(&native).ok().is_some_and(|native| {
        std::fs::canonicalize(&target.path).is_ok_and(|target| native == target)
    });
    // Old import markers contained no provenance. When a regular config
    // directory exists, old json/jsonc targets are the former import set.
    // Otherwise preserve potentially independent managed configuration.
    if manifest.is_none() && already_imported && native.is_dir() && !same_config {
        for name in ["opencode.json", "opencode.jsonc"] {
            if target.exists(name)? {
                mirrored.insert(name.into());
            }
        }
    }
    for (name, bytes) in sources.into_iter().filter(|_| !same_config) {
        if let Some(bytes) = bytes {
            target.write(name, &bytes)?;
            mirrored.insert(name.into());
        } else if mirrored.contains(name) {
            target.remove(name)?;
            mirrored.remove(name);
        }
    }
    // Keep the auth marker independent from config refresh. Logging out must
    // never resurrect the host's original provider login.
    if !already_imported {
        let auth = home.child("data")?.child("opencode")?;
        if !auth.exists("auth.json")? {
            if let Some(bytes) = source(&data.join("opencode/auth.json"))? {
                auth.write("auth.json", &bytes)?;
            }
        }
        home.write(".native-imported", b"1")?;
    }
    let bytes = serde_json::to_vec(&mirrored).map_err(io::Error::other)?;
    home.write(MANIFEST, &bytes)
}
