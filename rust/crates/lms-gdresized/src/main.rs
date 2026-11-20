#![deny(unsafe_op_in_unsafe_fn)]

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{DirBuilder, File, Metadata, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

const DEFAULT_SOCKET_PATH: &str = "/tmp/sbs_artwork";
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
const MAX_INPUT_FILE_BYTES: u64 = 64 * 1024 * 1024;
const SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(15);
const PRE_QUEUE_HEADER_TIMEOUT: Duration = Duration::from_secs(15);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_STORE_CACHE_ENTRIES: usize = 8;
const WORKER_COUNT: usize = 4;
const MAX_WORKER_RESTARTS: usize = 16;
const WORKER_QUEUE_CAPACITY: usize = 64;
const MAX_PENDING_CONNS: usize = 128;

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static SIGNAL_HANDLER_INIT: Once = Once::new();

extern "C" fn handle_shutdown_signal(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::Release);
}

fn install_signal_handlers() {
    SIGNAL_HANDLER_INIT.call_once(|| {
        let signals = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];
        // JUSTIFICATION: Register async-signal-safe shutdown handlers for
        // SIGHUP, SIGINT, SIGQUIT, and SIGTERM via POSIX sigaction(2).
        // SAFETY:
        // 1. Preconditions: Called at most once via Once during init.
        // 2. Bounds/Alignment: `sa` is zero-initialized `libc::sigaction` POD.
        // 3. Aliasing/Lifetime: Handler fn has static program lifetime.
        // 4. Async-signal safety: Handler only executes AtomicBool::store.
        // 5. Return value: Non-zero sigaction failures do not corrupt state.
        // 6. Failure mode: On failure, ppid/EOF checks still terminate loop.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = handle_shutdown_signal as *const () as usize;
            let _ = libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = 0;
            for sig in signals {
                let _ = libc::sigaction(sig, &sa, std::ptr::null_mut());
            }
        }
    });
}

fn sanitize_for_log(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_control() {
            out.push_str(&format!("\\x{:02x}", ch as u32));
        } else {
            out.push(ch);
        }
    }
    out
}

fn sanitize_path_for_log(path: &Path) -> String {
    sanitize_for_log(&path.to_string_lossy())
}

fn parse_specs(spec: &str) -> Vec<String> {
    let mut specs = Vec::new();
    for s in spec.split(',') {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            specs.push(trimmed.to_string());
        }
    }
    if specs.is_empty() {
        specs.push("XxX".to_string());
    }
    specs
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CliArgs {
    socket_path: Option<PathBuf>,
    faster: bool,
    debug: bool,
    max_requests: Option<usize>,
    cli_resize: bool,
    spec: Option<String>,
    file: Option<PathBuf>,
    stdin: bool,
    help: bool,
}

fn parse_cli_args<I, S>(args: I) -> Result<CliArgs, String>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut opts = CliArgs::default();
    let mut iter = args.into_iter();
    let _ = iter.next(); // skip argv[0]

    while let Some(arg_raw) = iter.next() {
        let arg_os: OsString = arg_raw.into();
        let arg = arg_os
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 argument: {:?}", arg_os))?;
        match arg {
            "--help" | "-h" => {
                opts.help = true;
            }
            "--socket" => {
                let val: OsString = iter
                    .next()
                    .ok_or_else(|| "--socket requires a path value".to_string())?
                    .into();
                if val.is_empty() {
                    return Err("--socket path must not be empty".to_string());
                }
                opts.socket_path = Some(PathBuf::from(val));
            }
            "--faster" => {
                opts.faster = true;
            }
            "--debug" => {
                opts.debug = true;
            }
            "--max-requests" => {
                let val_os: OsString = iter
                    .next()
                    .ok_or_else(|| "--max-requests requires a numeric value".to_string())?
                    .into();
                let val = val_os
                    .to_str()
                    .ok_or_else(|| "non-UTF-8 --max-requests value".to_string())?;
                let parsed = val
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --max-requests '{val}': {e}"))?;
                opts.max_requests = Some(parsed);
            }
            "--cli-resize" => {
                opts.cli_resize = true;
            }
            "--spec" => {
                let val_os: OsString = iter
                    .next()
                    .ok_or_else(|| "--spec requires a value".to_string())?
                    .into();
                let val = val_os
                    .into_string()
                    .map_err(|_| "non-UTF-8 --spec value".to_string())?;
                opts.spec = Some(val);
            }
            "--file" => {
                let val: OsString = iter
                    .next()
                    .ok_or_else(|| "--file requires a path value".to_string())?
                    .into();
                if val.is_empty() {
                    return Err("--file path must not be empty".to_string());
                }
                opts.file = Some(PathBuf::from(val));
            }
            "--stdin" => {
                opts.stdin = true;
            }
            other => {
                return Err(format!("unknown argument: {other}"));
            }
        }
    }
    if opts.file.is_some() && opts.stdin {
        return Err("cannot specify both --file and --stdin".to_string());
    }
    Ok(opts)
}

fn current_euid() -> std::io::Result<u32> {
    #[cfg(unix)]
    {
        // JUSTIFICATION: Query effective UID directly via POSIX geteuid(2).
        // SAFETY:
        // 1. Preconditions: Standard libc geteuid symbol is linked on Unix.
        // 2. Bounds/Alignment: No memory or pointer arguments are passed.
        // 3. Aliasing/Lifetime: No borrowed data crosses the FFI boundary.
        // 4. Thread safety: geteuid(2) is thread-safe and async-signal-safe.
        // 5. Return value: Returns valid uid_t effective user ID.
        // 6. Failure mode: POSIX geteuid(2) is documented as infallible.
        Ok(unsafe { libc::geteuid() })
    }
    #[cfg(not(unix))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "geteuid is only supported on Unix",
        ))
    }
}

struct ValidatedCacheRoot {
    path: PathBuf,
    _pinned_dir: File,
}

fn validate_cacheroot(cacheroot: &Path) -> std::io::Result<ValidatedCacheRoot> {
    if cacheroot.as_os_str().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cacheroot must not be empty",
        ));
    }
    if !cacheroot.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cacheroot must be an absolute path",
        ));
    }
    for comp in cacheroot.components() {
        if matches!(comp, Component::ParentDir | Component::CurDir) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cacheroot must not contain '.' or '..' components",
            ));
        }
    }

    let link_meta = std::fs::symlink_metadata(cacheroot)?;
    if link_meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cacheroot must not be a symlink",
        ));
    }
    if !link_meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cacheroot is not a directory",
        ));
    }
    let euid = current_euid()?;
    if link_meta.uid() != euid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cacheroot ownership does not match current user",
        ));
    }
    if (link_meta.permissions().mode() & 0o022) != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cacheroot must not be group- or world-writable",
        ));
    }

    // Pin the directory FD with O_DIRECTORY | O_NOFOLLOW before canonicalizing
    // and verify fstat(2) metadata against the canonical path.
    let mut dir_opts = OpenOptions::new();
    dir_opts.read(true);
    #[cfg(unix)]
    {
        dir_opts.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    }
    let pinned_dir = dir_opts.open(cacheroot)?;
    let fd_meta = pinned_dir.metadata()?;
    if !fd_meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cacheroot is not a directory",
        ));
    }
    if fd_meta.uid() != euid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cacheroot ownership does not match current user",
        ));
    }
    if (fd_meta.permissions().mode() & 0o022) != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cacheroot must not be group- or world-writable",
        ));
    }
    if fd_meta.dev() != link_meta.dev() || fd_meta.ino() != link_meta.ino() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cacheroot changed during validation",
        ));
    }

    let canonical = std::fs::canonicalize(cacheroot)?;
    if canonical != cacheroot {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cacheroot path must not traverse symlinks",
        ));
    }
    let canon_meta = std::fs::symlink_metadata(&canonical)?;
    if canon_meta.file_type().is_symlink() || !canon_meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "canonical cacheroot is not a non-symlink directory",
        ));
    }
    if canon_meta.uid() != fd_meta.uid()
        || canon_meta.permissions().mode() != fd_meta.permissions().mode()
        || canon_meta.dev() != fd_meta.dev()
        || canon_meta.ino() != fd_meta.ino()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cacheroot changed during validation",
        ));
    }
    Ok(ValidatedCacheRoot {
        path: canonical,
        _pinned_dir: pinned_dir,
    })
}

struct SocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl SocketGuard {
    fn new(path: PathBuf, dev: u64, ino: u64) -> Self {
        Self { path, dev, ino }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::symlink_metadata(&self.path) {
            if meta.file_type().is_socket() && meta.dev() == self.dev && meta.ino() == self.ino {
                if let Ok(euid) = current_euid() {
                    if meta.uid() == euid {
                        let _ = std::fs::remove_file(&self.path);
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingSocketState {
    NotFound,
    StaleDeadSocket,
}

fn inspect_existing_socket_path(
    sock_path: &Path,
) -> Result<ExistingSocketState, Box<dyn std::error::Error>> {
    match std::fs::symlink_metadata(sock_path) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() || !ft.is_socket() {
                return Err(format!(
                    "refusing to replace non-socket path: {}",
                    sock_path.display()
                )
                .into());
            }
            let euid = current_euid()?;
            if meta.uid() != euid {
                return Err(format!(
                    "refusing to replace socket owned by uid {}: {}",
                    meta.uid(),
                    sock_path.display()
                )
                .into());
            }
            match UnixStream::connect(sock_path) {
                Ok(_) => Err(format!(
                    "refusing to replace active socket in use by live peer: {}",
                    sock_path.display()
                )
                .into()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Ok(ExistingSocketState::NotFound)
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    Ok(ExistingSocketState::StaleDeadSocket)
                }
                Err(e) => Err(format!(
                    "failed probing existing socket {}: {e}",
                    sock_path.display()
                )
                .into()),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ExistingSocketState::NotFound),
        Err(e) => Err(Box::new(e)),
    }
}

fn rename_noreplace(src: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let staged_c = CString::new(src.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "source path contains interior NUL byte",
            )
        })?;
        let sock_c = CString::new(dst.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "destination path contains interior NUL byte",
            )
        })?;
        // JUSTIFICATION: Atomically place the staged socket without overwriting
        // any concurrently created path entry via Linux renameat2(RENAME_NOREPLACE).
        // SAFETY:
        // 1. Preconditions: `staged_c` and `sock_c` are valid NUL-terminated CStrings.
        // 2. Bounds/Alignment: `as_ptr()` yields valid read-only `*const c_char`.
        // 3. Aliasing/Lifetime: Both CStrings remain live for the syscall duration.
        // 4. Thread safety: `syscall(SYS_renameat2, ...)` is thread-safe in the kernel.
        // 5. Return value: Returns 0 on success or -1 with errno set on failure.
        // 6. Failure mode: Falls back to `std::fs::rename` only on `ENOSYS`.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                staged_c.as_ptr(),
                libc::AT_FDCWD,
                sock_c.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOSYS) {
            return std::fs::rename(src, dst);
        }
        Err(err)
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::fs::rename(src, dst)
    }
}

struct StagingDirGuard {
    path: PathBuf,
}

impl Drop for StagingDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn create_private_staging_dir(parent: &Path) -> std::io::Result<StagingDirGuard> {
    static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let euid = current_euid()?;
    for _ in 0..64 {
        let seq = STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir_path = parent.join(format!(".gdresized-sock-{pid}-{seq}"));
        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&dir_path) {
            Ok(()) => {
                let meta = std::fs::symlink_metadata(&dir_path)?;
                if meta.file_type().is_symlink() || !meta.is_dir() || meta.uid() != euid {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "staging directory ownership or type check failed",
                    ));
                }
                if (meta.permissions().mode() & 0o077) != 0 {
                    std::fs::set_permissions(&dir_path, PermissionsExt::from_mode(0o700))?;
                }
                return Ok(StagingDirGuard { path: dir_path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "failed to allocate unique socket staging directory",
    ))
}

fn bind_owner_only_socket(
    sock_path: &Path,
) -> Result<(UnixListener, SocketGuard), Box<dyn std::error::Error>> {
    inspect_existing_socket_path(sock_path)?;

    let parent = sock_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let staging = create_private_staging_dir(parent)?;

    let staged_sock = staging.path.join("s");
    let listener = UnixListener::bind(&staged_sock)?;
    std::fs::set_permissions(&staged_sock, PermissionsExt::from_mode(0o600))?;

    // Re-verify immediately before publishing without any unlink-to-rename gap.
    match inspect_existing_socket_path(sock_path)? {
        ExistingSocketState::StaleDeadSocket => {
            std::fs::rename(&staged_sock, sock_path)?;
        }
        ExistingSocketState::NotFound => {
            rename_noreplace(&staged_sock, sock_path)?;
        }
    }

    let meta = std::fs::symlink_metadata(sock_path)?;
    let guard = SocketGuard::new(sock_path.to_path_buf(), meta.dev(), meta.ino());
    Ok((listener, guard))
}

pub fn resize_one_shot(
    input: &[u8],
    spec: &str,
    faster: bool,
) -> Result<(String, Vec<u8>), Box<dyn std::error::Error>> {
    let specs = parse_specs(spec);
    let primary_spec = specs.first().map(String::as_str).unwrap_or("XxX");
    let parsed_spec = lms_image::parse_single_spec(primary_spec);
    let (bytes, format) = lms_image::resize_image(input, &parsed_spec, faster)?;
    Ok((format.as_str().to_string(), bytes))
}

type StoreCacheKey = (PathBuf, bool);
type StoreCacheMap = HashMap<StoreCacheKey, Arc<lms_cache::DbArtworkStore>>;

static STORE_CACHE: OnceLock<Mutex<StoreCacheMap>> = OnceLock::new();

fn evict_cached_store(canonical_root: &Path, is_imageproxy: bool) {
    let Some(cache) = STORE_CACHE.get() else {
        return;
    };
    if let Ok(mut guard) = cache.lock() {
        guard.remove(&(canonical_root.to_path_buf(), is_imageproxy));
    }
}

fn is_sqlite_or_cache_error(err: &lms_image::ImageError) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(curr) = source {
        if curr.is::<lms_cache::CacheError>() {
            return true;
        }
        source = curr.source();
    }
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("sqlite")
        || msg.contains("database")
        || msg.contains("cache")
        || msg.contains("readonly")
        || msg.contains("disk i/o")
}

pub fn open_or_cached_store(
    cacheroot: &Path,
    is_imageproxy: bool,
) -> Result<Arc<lms_cache::DbArtworkStore>, lms_cache::CacheError> {
    let validated_root = validate_cacheroot(cacheroot)?;
    let cache = STORE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (validated_root.path.clone(), is_imageproxy);

    {
        let guard = cache.lock().map_err(|_| {
            lms_cache::CacheError::Io(std::io::Error::other("store cache mutex poisoned"))
        })?;
        if let Some(store) = guard.get(&key) {
            return Ok(Arc::clone(store));
        }
    }

    let store = Arc::new(lms_cache::DbArtworkStore::open(
        &validated_root.path,
        is_imageproxy,
    )?);

    let mut guard = cache.lock().map_err(|_| {
        lms_cache::CacheError::Io(std::io::Error::other("store cache mutex poisoned"))
    })?;
    if let Some(existing) = guard.get(&key) {
        return Ok(Arc::clone(existing));
    }
    while guard.len() >= MAX_STORE_CACHE_ENTRIES {
        let Some(evict_key) = guard.keys().next().cloned() else {
            break;
        };
        guard.remove(&evict_key);
    }
    guard.insert(key, Arc::clone(&store));
    Ok(store)
}

fn append_frame_chunk(raw_line: &mut Vec<u8>, slice: &[u8]) -> Result<bool, String> {
    if let Some(pos) = slice.iter().position(|&b| b == b'\n') {
        let needed = pos + 1;
        if raw_line.len().saturating_add(needed) > MAX_FRAME_BYTES {
            return Err("frame exceeded 32 MiB limit".to_string());
        }
        let Some(head) = slice.get(..needed) else {
            return Err("invalid frame slice".to_string());
        };
        raw_line.extend_from_slice(head);
        return Ok(true);
    }
    if raw_line.len().saturating_add(slice.len()) >= MAX_FRAME_BYTES {
        return Err("frame exceeded 32 MiB limit without newline".to_string());
    }
    raw_line.extend_from_slice(slice);
    Ok(false)
}

fn read_frame_with_deadline(
    stream: &mut UnixStream,
    deadline: Instant,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT))?;
    let mut raw_line = Vec::new();
    let mut chunk = [0_u8; 8192];

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err("frame read exceeded 15s deadline".into());
        }
        let remaining = deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            return Err("frame read exceeded 15s deadline".into());
        }
        stream.set_read_timeout(Some(remaining.max(Duration::from_millis(1))))?;

        match stream.read(&mut chunk) {
            Ok(0) => {
                if raw_line.is_empty() {
                    return Err("empty frame".into());
                }
                return Err("frame missing trailing newline delimiter".into());
            }
            Ok(n) => {
                let Some(slice) = chunk.get(..n) else {
                    return Err("invalid socket read length".into());
                };
                if append_frame_chunk(&mut raw_line, slice)? {
                    return Ok(raw_line);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err("frame read timed out after 15s".into());
            }
            Err(e) => return Err(Box::new(e)),
        }
    }
}

enum InputReadError {
    EmptyPath,
    RelativePath,
    InvalidPathComponent,
    Symlink,
    ChangedDuringValidation,
    Open(std::io::Error),
    Stat(std::io::Error),
    NotRegularFile,
    TooLarge,
    Read(std::io::Error),
    Seek(std::io::Error),
    Extract(lms_image::ImageError),
    NoArtwork,
    EmptyArtwork,
}

fn open_validated_regular_file(path: &Path) -> Result<(File, Metadata, PathBuf), InputReadError> {
    if path.as_os_str().is_empty() {
        return Err(InputReadError::EmptyPath);
    }
    #[cfg(unix)]
    if !path.is_absolute() {
        return Err(InputReadError::RelativePath);
    }
    for comp in path.components() {
        if matches!(comp, Component::ParentDir | Component::CurDir) {
            return Err(InputReadError::InvalidPathComponent);
        }
    }
    let link_meta = std::fs::symlink_metadata(path).map_err(InputReadError::Stat)?;
    if link_meta.file_type().is_symlink() {
        return Err(InputReadError::Symlink);
    }
    if !link_meta.is_file() {
        return Err(InputReadError::NotRegularFile);
    }
    let canonical = std::fs::canonicalize(path).map_err(InputReadError::Stat)?;
    if canonical != path {
        return Err(InputReadError::Symlink);
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(&canonical).map_err(InputReadError::Open)?;
    // File::metadata() issues fstat(2) on the opened descriptor.
    let meta = file.metadata().map_err(InputReadError::Stat)?;
    if !meta.is_file() {
        return Err(InputReadError::NotRegularFile);
    }
    if meta.dev() != link_meta.dev() || meta.ino() != link_meta.ino() {
        return Err(InputReadError::ChangedDuringValidation);
    }
    if meta.len() > MAX_INPUT_FILE_BYTES {
        return Err(InputReadError::TooLarge);
    }
    Ok((file, meta, canonical))
}

fn read_regular_input_bytes(path: &Path) -> Result<Vec<u8>, InputReadError> {
    let (mut file, meta, _) = open_validated_regular_file(path)?;
    let file_len = meta.len();

    let mut header = [0_u8; 64];
    let mut hdr_len = 0_usize;
    while hdr_len < header.len() {
        let Some(dst) = header.get_mut(hdr_len..) else {
            break;
        };
        match file.read(dst) {
            Ok(0) => break,
            Ok(n) => hdr_len += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(InputReadError::Read(e)),
        }
    }

    let Some(prefix) = header.get(..hdr_len) else {
        return Err(InputReadError::Read(std::io::Error::other(
            "invalid header slice length",
        )));
    };
    if lms_image::sniff_format(prefix).is_some() {
        let cap = usize::try_from(file_len).unwrap_or(0);
        let mut raw = Vec::with_capacity(cap.max(hdr_len));
        raw.extend_from_slice(prefix);
        let remaining_limit = MAX_INPUT_FILE_BYTES
            .saturating_add(1)
            .saturating_sub(hdr_len as u64);
        std::io::Read::by_ref(&mut file)
            .take(remaining_limit)
            .read_to_end(&mut raw)
            .map_err(InputReadError::Read)?;
        if raw.len() as u64 > MAX_INPUT_FILE_BYTES {
            return Err(InputReadError::TooLarge);
        }
        return Ok(raw);
    }

    file.seek(SeekFrom::Start(0))
        .map_err(InputReadError::Seek)?;
    let mut bounded = std::io::Read::by_ref(&mut file).take(MAX_INPUT_FILE_BYTES);
    match lms_image::extract_embedded_artwork_from_reader(&mut bounded, file_len) {
        Ok(Some(art)) if !art.data.is_empty() => Ok(art.data),
        Ok(Some(_)) => Err(InputReadError::EmptyArtwork),
        Ok(None) => Err(InputReadError::NoArtwork),
        Err(e) => Err(InputReadError::Extract(e)),
    }
}

fn extract_image_from_bytes(buf: &[u8]) -> Result<Cow<'_, [u8]>, String> {
    if buf.is_empty() {
        return Err("stdin input is empty".to_string());
    }
    let prefix_len = buf.len().min(64);
    let Some(prefix) = buf.get(..prefix_len) else {
        return Err("invalid input slice".to_string());
    };
    if lms_image::sniff_format(prefix).is_some() {
        return Ok(Cow::Borrowed(buf));
    }
    let len = u64::try_from(buf.len()).unwrap_or(MAX_INPUT_FILE_BYTES);
    let mut bounded = Cursor::new(buf).take(MAX_INPUT_FILE_BYTES);
    match lms_image::extract_embedded_artwork_from_reader(&mut bounded, len) {
        Ok(Some(art)) if !art.data.is_empty() => Ok(Cow::Owned(art.data)),
        Ok(Some(_)) => Err("extracted embedded artwork is empty".to_string()),
        Ok(None) => Err("no embedded artwork found".to_string()),
        Err(e) => Err(format!("embedded artwork extraction failed: {e}")),
    }
}

fn format_client_input_error(file: &Path, err: InputReadError) -> String {
    let display = file.to_string_lossy();
    match err {
        InputReadError::EmptyPath => "input path must not be empty".to_string(),
        InputReadError::RelativePath => format!("input path must be absolute: {display}"),
        InputReadError::InvalidPathComponent => {
            format!("input path must not contain '.' or '..' components: {display}")
        }
        InputReadError::Symlink => format!("input path must not traverse symlinks: {display}"),
        InputReadError::ChangedDuringValidation => {
            format!("input path changed during validation: {display}")
        }
        InputReadError::Open(e) => format!("file open failed for {display}: {e}"),
        InputReadError::Stat(e) => format!("file stat failed for {display}: {e}"),
        InputReadError::NotRegularFile => format!("input path is not a regular file: {display}"),
        InputReadError::TooLarge => format!("input file exceeds 64 MiB limit: {display}"),
        InputReadError::Read(e) => format!("file read failed for {display}: {e}"),
        InputReadError::Seek(e) => format!("file seek failed for {display}: {e}"),
        InputReadError::Extract(e) => {
            format!("embedded artwork extraction failed for {display}: {e}")
        }
        InputReadError::NoArtwork => format!("no embedded artwork found in {display}"),
        InputReadError::EmptyArtwork => format!("extracted embedded artwork is empty in {display}"),
    }
}

fn handle_client(
    stream: &mut UnixStream,
    pre_read_frame: Option<Vec<u8>>,
    deadline: Instant,
    faster: bool,
    debug: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if Instant::now() >= deadline {
        return Err("connection deadline exceeded".into());
    }
    let raw_line = match pre_read_frame {
        Some(frame) => frame,
        None => read_frame_with_deadline(stream, deadline)?,
    };
    let raw = raw_line
        .strip_suffix(b"\r\n")
        .or_else(|| raw_line.strip_suffix(b"\n"))
        .ok_or("frame missing trailing newline delimiter")?;

    let mut parts = raw.splitn(6, |&b| b == 0);
    let (
        Some(file_bytes),
        Some(spec_bytes),
        Some(cacheroot_bytes),
        Some(cachekey_bytes),
        Some(data_bytes),
        Some(trailing_bytes),
    ) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    )
    else {
        return Err("malformed frame: fewer than 5 null-terminated fields".into());
    };

    if !trailing_bytes.is_empty() {
        return Err("malformed frame: unexpected trailing data after 5th NUL".into());
    }

    let file_path = Path::new(OsStr::from_bytes(file_bytes));
    let mut spec =
        std::str::from_utf8(spec_bytes).map_err(|e| format!("invalid UTF-8 in spec: {e}"))?;
    let cacheroot_path = Path::new(OsStr::from_bytes(cacheroot_bytes));
    let cachekey = std::str::from_utf8(cachekey_bytes)
        .map_err(|e| format!("invalid UTF-8 in cachekey: {e}"))?;
    let data_str =
        std::str::from_utf8(data_bytes).map_err(|e| format!("invalid UTF-8 in data: {e}"))?;

    if spec.is_empty() {
        spec = "XxX";
    }

    if file_bytes.is_empty() || cacheroot_bytes.is_empty() || cachekey.is_empty() {
        return Err("missing required parameters".into());
    }

    let validated_root = validate_cacheroot(cacheroot_path).map_err(|e| {
        format!(
            "invalid cacheroot '{}': {e}",
            cacheroot_path.to_string_lossy()
        )
    })?;

    let trimmed_data = data_str.trim();
    enum ResolvedInput {
        Inline(Vec<u8>),
        OpenedFile {
            file: File,
            original_path: PathBuf,
            mtime: i64,
        },
    }
    let resolved_input = if !trimmed_data.is_empty() {
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(trimmed_data.as_bytes())
            .map_err(|e| format!("invalid base64 data: {e}"))?;
        if decoded.is_empty() {
            return Err("decoded inline image data is empty".into());
        }
        ResolvedInput::Inline(decoded)
    } else if file_bytes == b"data" {
        return Err("missing inline image data".into());
    } else {
        let (file, meta, original_path) = open_validated_regular_file(file_path)
            .map_err(|err| format_client_input_error(file_path, err))?;
        let mtime = meta.mtime();
        ResolvedInput::OpenedFile {
            file,
            original_path,
            mtime,
        }
    };

    let inline_len = match &resolved_input {
        ResolvedInput::Inline(bytes) => bytes.len(),
        ResolvedInput::OpenedFile { .. } => 0,
    };

    let specs = parse_specs(spec);

    let is_imageproxy = cachekey.starts_with("imageproxy");
    if debug {
        eprintln!(
            "file={}, spec={}, cacheroot={}, cachekey={}, \
             imageproxy={is_imageproxy}, imagedata={inline_len} bytes",
            sanitize_path_for_log(file_path),
            sanitize_for_log(spec),
            sanitize_path_for_log(&validated_root.path),
            sanitize_for_log(cachekey),
        );
    }

    let store = open_or_cached_store(&validated_root.path, is_imageproxy)?;
    let resize_res = match resolved_input {
        ResolvedInput::Inline(ref bytes) => lms_image::gdresize_to_cache(
            lms_image::InputSource::Bytes(bytes),
            &specs,
            &store,
            cachekey,
            faster,
        ),
        ResolvedInput::OpenedFile {
            file,
            original_path,
            mtime,
        } => lms_image::gdresize_to_cache(
            lms_image::InputSource::OpenedFile {
                file,
                original_path,
                mtime,
            },
            &specs,
            &store,
            cachekey,
            faster,
        ),
    };
    if let Err(err) = resize_res {
        if is_sqlite_or_cache_error(&err) {
            evict_cached_store(&validated_root.path, is_imageproxy);
        }
        return Err(Box::new(err));
    }

    Ok(())
}

#[inline]
pub fn process_client_request(
    stream: &mut UnixStream,
    faster: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + SOCKET_IO_TIMEOUT;
    handle_client(stream, None, deadline, faster, false)
}

fn send_socket_response(stream: &mut UnixStream, response: &[u8]) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT));
    let _ = stream.write_all(response);
    let _ = stream.flush();
}

struct RequestReplyGuard {
    stream: UnixStream,
    completed: bool,
}

impl RequestReplyGuard {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            completed: false,
        }
    }

    fn stream_mut(&mut self) -> &mut UnixStream {
        &mut self.stream
    }

    fn mark_completed(&mut self) {
        self.completed = true;
    }
}

impl Drop for RequestReplyGuard {
    fn drop(&mut self) {
        if !self.completed {
            send_socket_response(&mut self.stream, b"Error: internal worker error\r\n");
        }
    }
}

fn handle_client_connection(req: ClientRequest, faster: bool, debug: bool) {
    let started = debug.then(Instant::now);
    #[cfg(test)]
    let panic_hook = req.panic_hook;
    let ClientRequest {
        stream,
        pre_read_frame,
        deadline,
        ..
    } = req;
    let mut reply_guard = RequestReplyGuard::new(stream);
    #[cfg(test)]
    if let Some(hook) = panic_hook {
        hook();
    }
    let res = handle_client(
        reply_guard.stream_mut(),
        pre_read_frame,
        deadline,
        faster,
        debug,
    );
    let response = match res {
        Ok(()) => {
            if let Some(t0) = started {
                eprintln!("OK ({:.3}s)", t0.elapsed().as_secs_f64());
            }
            "OK\r\n".to_string()
        }
        Err(e) => {
            if debug {
                eprintln!("Error: {}", sanitize_for_log(&e.to_string()));
            }
            format!("Error: {e}\r\n")
        }
    };
    send_socket_response(reply_guard.stream_mut(), response.as_bytes());
    reply_guard.mark_completed();
}

pub fn run_stdio(spec: &str, faster: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = Vec::new();
    std::io::stdin()
        .take(MAX_INPUT_FILE_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_INPUT_FILE_BYTES {
        return Err("stdin input exceeded 64MB limit".into());
    }
    let image_bytes = extract_image_from_bytes(&buf)?;
    let (fmt, bytes) = resize_one_shot(&image_bytes, spec, faster)?;
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{fmt}")?;
    stdout.write_all(&bytes)?;
    stdout.flush()?;
    Ok(())
}

struct ClientRequest {
    stream: UnixStream,
    pre_read_frame: Option<Vec<u8>>,
    deadline: Instant,
    #[cfg(test)]
    panic_hook: Option<fn()>,
}

impl ClientRequest {
    fn new(stream: UnixStream, pre_read_frame: Option<Vec<u8>>, deadline: Instant) -> Self {
        Self {
            stream,
            pre_read_frame,
            deadline,
            #[cfg(test)]
            panic_hook: None,
        }
    }

    #[cfg(test)]
    fn with_panic_hook(
        stream: UnixStream,
        pre_read_frame: Option<Vec<u8>>,
        deadline: Instant,
        panic_hook: fn(),
    ) -> Self {
        Self {
            stream,
            pre_read_frame,
            deadline,
            panic_hook: Some(panic_hook),
        }
    }
}

enum EnqueueError {
    Full(ClientRequest),
    Closed(ClientRequest),
}

struct QueueState {
    items: VecDeque<ClientRequest>,
    closed: bool,
}

struct WorkerQueue {
    state: Mutex<QueueState>,
    cv: Condvar,
}

impl WorkerQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(QueueState {
                items: VecDeque::with_capacity(WORKER_QUEUE_CAPACITY),
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }

    fn try_enqueue(&self, req: ClientRequest) -> Result<(), EnqueueError> {
        let Ok(mut state) = self.state.lock() else {
            return Err(EnqueueError::Closed(req));
        };
        if state.closed {
            return Err(EnqueueError::Closed(req));
        }
        if state.items.len() >= WORKER_QUEUE_CAPACITY {
            return Err(EnqueueError::Full(req));
        }
        state.items.push_back(req);
        self.cv.notify_one();
        Ok(())
    }

    fn pop_wait(&self) -> Result<Option<ClientRequest>, &'static str> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "worker queue mutex poisoned")?;
        loop {
            if let Some(req) = state.items.pop_front() {
                return Ok(Some(req));
            }
            if state.closed {
                return Ok(None);
            }
            state = self
                .cv
                .wait(state)
                .map_err(|_| "worker queue mutex poisoned")?;
        }
    }

    fn is_closed(&self) -> bool {
        self.state.lock().map(|s| s.closed).unwrap_or(true)
    }

    fn close(&self) -> Result<(), &'static str> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "worker queue mutex poisoned")?;
        state.closed = true;
        self.cv.notify_all();
        Ok(())
    }
}

struct WorkerRuntime {
    queue: Arc<WorkerQueue>,
    shutdown: AtomicBool,
    restarts: AtomicUsize,
    unrecovered_panic: AtomicBool,
    handles: Mutex<Vec<std::thread::JoinHandle<Result<(), &'static str>>>>,
    faster: bool,
    debug: bool,
}

struct WorkerSentinel {
    runtime: Arc<WorkerRuntime>,
    disarmed: bool,
}

impl WorkerSentinel {
    fn new(runtime: Arc<WorkerRuntime>) -> Self {
        Self {
            runtime,
            disarmed: false,
        }
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for WorkerSentinel {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        let Ok(mut handles) = self.runtime.handles.lock() else {
            self.runtime
                .unrecovered_panic
                .store(true, Ordering::Release);
            return;
        };
        if self.runtime.shutdown.load(Ordering::Acquire) || self.runtime.queue.is_closed() {
            return;
        }

        let mut current = self.runtime.restarts.load(Ordering::Acquire);
        loop {
            if current >= MAX_WORKER_RESTARTS {
                if self.runtime.debug {
                    eprintln!("worker restart cap ({MAX_WORKER_RESTARTS}) reached; not respawning");
                }
                self.runtime
                    .unrecovered_panic
                    .store(true, Ordering::Release);
                return;
            }
            match self.runtime.restarts.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }

        let restart_num = current + 1;
        if self.runtime.debug {
            eprintln!(
                "worker exited unexpectedly; respawning replacement \
                 ({restart_num}/{MAX_WORKER_RESTARTS})"
            );
        }
        let replacement_runtime = Arc::clone(&self.runtime);
        let thread_name = format!("gdresized-worker-r{restart_num}");
        match std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || worker_loop(replacement_runtime))
        {
            Ok(handle) => handles.push(handle),
            Err(e) => {
                if self.runtime.debug {
                    eprintln!(
                        "failed to respawn replacement worker: {}",
                        sanitize_for_log(&e.to_string())
                    );
                }
                self.runtime
                    .unrecovered_panic
                    .store(true, Ordering::Release);
            }
        }
    }
}

fn worker_loop(runtime: Arc<WorkerRuntime>) -> Result<(), &'static str> {
    let mut sentinel = WorkerSentinel::new(Arc::clone(&runtime));
    loop {
        match runtime.queue.pop_wait() {
            Ok(Some(req)) => {
                handle_client_connection(req, runtime.faster, runtime.debug);
            }
            Ok(None) => {
                sentinel.disarm();
                return Ok(());
            }
            Err(msg) => {
                sentinel.disarm();
                return Err(msg);
            }
        }
    }
}

struct WorkerPool {
    runtime: Arc<WorkerRuntime>,
}

impl WorkerPool {
    fn spawn(faster: bool, debug: bool) -> Result<Self, Box<dyn std::error::Error>> {
        let runtime = Arc::new(WorkerRuntime {
            queue: Arc::new(WorkerQueue::new()),
            shutdown: AtomicBool::new(false),
            restarts: AtomicUsize::new(0),
            unrecovered_panic: AtomicBool::new(false),
            handles: Mutex::new(Vec::with_capacity(WORKER_COUNT)),
            faster,
            debug,
        });
        let mut initial_handles = Vec::with_capacity(WORKER_COUNT);
        for idx in 0..WORKER_COUNT {
            let worker_runtime = Arc::clone(&runtime);
            let handle = std::thread::Builder::new()
                .name(format!("gdresized-worker-{idx}"))
                .spawn(move || worker_loop(worker_runtime))?;
            initial_handles.push(handle);
        }
        {
            let mut guard = runtime
                .handles
                .lock()
                .map_err(|_| "worker handles mutex poisoned")?;
            guard.extend(initial_handles);
        }
        Ok(Self { runtime })
    }

    fn try_enqueue(&self, req: ClientRequest) -> Result<(), EnqueueError> {
        self.runtime.queue.try_enqueue(req)
    }

    #[cfg(test)]
    fn restart_count(&self) -> usize {
        self.runtime.restarts.load(Ordering::Acquire)
    }

    fn shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.shutdown.store(true, Ordering::Release);
        let close_res = self.runtime.queue.close();
        let mut first_err: Option<Box<dyn std::error::Error>> =
            close_res.err().map(|msg| msg.into());
        loop {
            let batch: Vec<_> = match self.runtime.handles.lock() {
                Ok(mut guard) => guard.drain(..).collect(),
                Err(_) => {
                    if first_err.is_none() {
                        first_err = Some("worker handles mutex poisoned".into());
                    }
                    break;
                }
            };
            if batch.is_empty() {
                break;
            }
            for handle in batch {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(msg)) => {
                        if first_err.is_none() {
                            first_err = Some(msg.into());
                        }
                    }
                    Err(_) => {
                        if self.runtime.unrecovered_panic.load(Ordering::Acquire)
                            && first_err.is_none()
                        {
                            first_err = Some("worker thread panicked".into());
                        }
                    }
                }
            }
        }
        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.runtime.shutdown.store(true, Ordering::Release);
        let _ = self.runtime.queue.close();
        loop {
            let batch: Vec<_> = match self.runtime.handles.lock() {
                Ok(mut guard) => guard.drain(..).collect(),
                Err(_) => break,
            };
            if batch.is_empty() {
                break;
            }
            for handle in batch {
                let _ = handle.join();
            }
        }
    }
}

struct PendingConnection {
    stream: UnixStream,
    deadline: Instant,
    raw_line: Vec<u8>,
}

enum PollOutcome {
    Pending(PendingConnection),
    Ready(ClientRequest),
    Failed(UnixStream, String),
}

impl PendingConnection {
    fn new(stream: UnixStream, deadline: Instant) -> Self {
        Self {
            stream,
            deadline,
            raw_line: Vec::new(),
        }
    }

    fn poll_read(mut self) -> PollOutcome {
        if Instant::now() >= self.deadline {
            return PollOutcome::Failed(self.stream, "frame read timed out after 15s".to_string());
        }

        let mut chunk = [0_u8; 8192];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    let msg = if self.raw_line.is_empty() {
                        "empty frame".to_string()
                    } else {
                        "frame missing trailing newline delimiter".to_string()
                    };
                    return PollOutcome::Failed(self.stream, msg);
                }
                Ok(n) => {
                    let Some(slice) = chunk.get(..n) else {
                        return PollOutcome::Failed(
                            self.stream,
                            "invalid socket read length".to_string(),
                        );
                    };
                    match append_frame_chunk(&mut self.raw_line, slice) {
                        Ok(true) => {
                            return PollOutcome::Ready(ClientRequest::new(
                                self.stream,
                                Some(self.raw_line),
                                self.deadline,
                            ));
                        }
                        Ok(false) => {
                            if Instant::now() >= self.deadline {
                                return PollOutcome::Failed(
                                    self.stream,
                                    "frame read timed out after 15s".to_string(),
                                );
                            }
                        }
                        Err(msg) => return PollOutcome::Failed(self.stream, msg),
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    return PollOutcome::Pending(self);
                }
                Err(e) => return PollOutcome::Failed(self.stream, e.to_string()),
            }
        }
    }
}

fn dispatch_ready_request(
    req: ClientRequest,
    pool: Option<&WorkerPool>,
    faster: bool,
    debug: bool,
) -> bool {
    if let Some(worker_pool) = pool {
        match worker_pool.try_enqueue(req) {
            Ok(()) => true,
            Err(EnqueueError::Full(mut busy)) => {
                send_socket_response(&mut busy.stream, b"Error: server busy\r\n");
                true
            }
            Err(EnqueueError::Closed(mut closed)) => {
                send_socket_response(&mut closed.stream, b"Error: server shutting down\r\n");
                false
            }
        }
    } else {
        handle_client_connection(req, faster, debug);
        true
    }
}

fn poll_pending_connections(
    pending: &mut Vec<PendingConnection>,
    pool: Option<&WorkerPool>,
    faster: bool,
    debug: bool,
) -> (bool, bool) {
    let mut made_progress = false;
    let mut keep_running = true;
    let mut next_pending = Vec::with_capacity(pending.len());

    for conn in pending.drain(..) {
        match conn.poll_read() {
            PollOutcome::Pending(kept) => {
                next_pending.push(kept);
            }
            PollOutcome::Ready(req) => {
                made_progress = true;
                if let Err(e) = req.stream.set_nonblocking(false) {
                    if debug {
                        eprintln!(
                            "failed to set blocking stream mode: {}",
                            sanitize_for_log(&e.to_string())
                        );
                    }
                    continue;
                }
                let _ = req.stream.set_read_timeout(Some(SOCKET_IO_TIMEOUT));
                let _ = req.stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT));
                if !dispatch_ready_request(req, pool, faster, debug) {
                    keep_running = false;
                }
            }
            PollOutcome::Failed(mut stream, msg) => {
                made_progress = true;
                let err_line = format!("Error: {msg}\r\n");
                send_socket_response(&mut stream, err_line.as_bytes());
            }
        }
    }
    *pending = next_pending;
    (made_progress, keep_running)
}

fn should_exit_server_with_flag(initial_ppid: u32, shutdown_flag: &AtomicBool) -> bool {
    if shutdown_flag.load(Ordering::Acquire) {
        return true;
    }
    if initial_ppid != 1 && std::os::unix::process::parent_id() != initial_ppid {
        return true;
    }
    false
}

fn run_server_with_shutdown(
    sock_path: &Path,
    faster: bool,
    max_requests: Option<usize>,
    debug: bool,
    shutdown_flag: &AtomicBool,
    install_signals: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if max_requests == Some(0) {
        return Ok(());
    }
    if install_signals && max_requests.is_none() {
        install_signal_handlers();
    }

    let (listener, _socket_guard) = bind_owner_only_socket(sock_path)?;
    listener.set_nonblocking(true)?;

    if debug {
        eprintln!("listening on {}", sanitize_path_for_log(sock_path));
    }

    let mut accepted: usize = 0;
    let initial_ppid = std::os::unix::process::parent_id();
    let mut pool = if max_requests.is_none() {
        Some(WorkerPool::spawn(faster, debug)?)
    } else {
        None
    };
    let mut pending: Vec<PendingConnection> = Vec::new();
    let mut fatal_accept_err: Option<std::io::Error> = None;

    loop {
        if should_exit_server_with_flag(initial_ppid, shutdown_flag) {
            break;
        }

        let mut accept_progress = false;
        while max_requests.is_none_or(|max| accepted < max)
            && !should_exit_server_with_flag(initial_ppid, shutdown_flag)
        {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    accept_progress = true;
                    accepted += 1;
                    let deadline = Instant::now() + PRE_QUEUE_HEADER_TIMEOUT;
                    if pending.len() >= MAX_PENDING_CONNS {
                        send_socket_response(&mut stream, b"Error: server busy\r\n");
                        continue;
                    }
                    if let Err(e) = stream.set_nonblocking(true) {
                        if debug {
                            eprintln!(
                                "failed to set nonblocking stream mode: {}",
                                sanitize_for_log(&e.to_string())
                            );
                        }
                        continue;
                    }
                    pending.push(PendingConnection::new(stream, deadline));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    fatal_accept_err = Some(e);
                    break;
                }
            }
        }

        let (poll_progress, keep_running) =
            poll_pending_connections(&mut pending, pool.as_ref(), faster, debug);

        if fatal_accept_err.is_some()
            || !keep_running
            || should_exit_server_with_flag(initial_ppid, shutdown_flag)
        {
            break;
        }
        if let Some(max) = max_requests {
            if accepted >= max && pending.is_empty() {
                break;
            }
        }
        if !accept_progress && !poll_progress {
            std::thread::sleep(ACCEPT_POLL_INTERVAL);
        }
    }

    // Drain accepted connections cleanly before shutting down workers.
    while !pending.is_empty() {
        if fatal_accept_err.is_none() && !should_exit_server_with_flag(initial_ppid, shutdown_flag)
        {
            let (poll_progress, keep_running) =
                poll_pending_connections(&mut pending, pool.as_ref(), faster, debug);
            if !keep_running {
                break;
            }
            if !pending.is_empty() && !poll_progress {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
        } else {
            let _ = poll_pending_connections(&mut pending, pool.as_ref(), faster, debug);
            for mut conn in pending.drain(..) {
                send_socket_response(&mut conn.stream, b"Error: server shutting down\r\n");
            }
        }
    }

    let shutdown_res = if let Some(ref mut worker_pool) = pool {
        worker_pool.shutdown()
    } else {
        Ok(())
    };

    if let Some(err) = fatal_accept_err {
        return Err(format!("listener accept failed: {err}").into());
    }
    shutdown_res
}

fn run_server_internal(
    sock_path: &Path,
    faster: bool,
    max_requests: Option<usize>,
    debug: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    run_server_with_shutdown(
        sock_path,
        faster,
        max_requests,
        debug,
        &SHUTDOWN_REQUESTED,
        true,
    )
}

pub fn run_server(
    sock_path: &Path,
    faster: bool,
    max_requests: Option<usize>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_server_internal(sock_path, faster, max_requests, false)
}

pub fn run_server_n_requests(
    sock_path: &Path,
    faster: bool,
    n: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    run_server(sock_path, faster, Some(n))
}

fn print_usage() {
    println!("Usage: gdresized [options]");
    println!("Options:");
    println!("  --socket <path>       UNIX domain socket path (default: /tmp/sbs_artwork)");
    println!("  --faster              Use faster resizing algorithm");
    println!("  --debug               Print diagnostic logs to stderr");
    println!("  --max-requests <num>  Exit after processing num requests");
    println!("  --cli-resize          One-shot CLI resize mode");
    println!("  --spec <spec>         Resize specification");
    println!("  --file <path>         Input image file");
    println!("  --stdin               Read input image from stdin");
    println!("  -h, --help            Display this help message");
}

fn read_cli_input_image(path: &Path) -> Result<Vec<u8>, String> {
    read_regular_input_bytes(path)
        .map_err(|err| format!("Error: {}", format_client_input_error(path, err)))
}

fn main() {
    let opts = match parse_cli_args(std::env::args_os()) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("Error parsing arguments: {e}");
            std::process::exit(2);
        }
    };
    if opts.help {
        print_usage();
        return;
    }

    if opts.cli_resize {
        let spec = opts.spec.as_deref().unwrap_or("XxX");
        let image_bytes = if let Some(ref path) = opts.file {
            match read_cli_input_image(path) {
                Ok(bytes) => bytes,
                Err(msg) => {
                    eprintln!("{msg}");
                    std::process::exit(1);
                }
            }
        } else if opts.stdin {
            if let Err(e) = run_stdio(spec, opts.faster) {
                eprintln!("Error reading stdin: {e}");
                std::process::exit(1);
            }
            return;
        } else {
            eprintln!("Error: --cli-resize requires either --file <path> or --stdin");
            std::process::exit(1);
        };

        match resize_one_shot(&image_bytes, spec, opts.faster) {
            Ok((fmt, bytes)) => {
                let mut stdout = std::io::stdout();
                if let Err(e) = writeln!(stdout, "{fmt}") {
                    eprintln!("Error writing stdout: {e}");
                    std::process::exit(1);
                }
                if let Err(e) = stdout.write_all(&bytes) {
                    eprintln!("Error writing stdout: {e}");
                    std::process::exit(1);
                }
                if let Err(e) = stdout.flush() {
                    eprintln!("Error flushing stdout: {e}");
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("Error resizing image: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    install_signal_handlers();
    let socket_path = opts
        .socket_path
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH));

    if let Err(e) = run_server_internal(&socket_path, opts.faster, opts.max_requests, opts.debug) {
        eprintln!("Daemon error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::dispatch_ready_request;
    use super::evict_cached_store;
    use super::extract_image_from_bytes;
    use super::handle_shutdown_signal;
    use super::open_or_cached_store;
    use super::parse_cli_args;
    use super::read_cli_input_image;
    use super::resize_one_shot;
    use super::run_server_n_requests;
    use super::run_server_with_shutdown;
    use super::sanitize_for_log;
    use super::should_exit_server_with_flag;
    use super::ClientRequest;
    use super::EnqueueError;
    use super::SocketGuard;
    use super::WorkerPool;
    use super::WorkerQueue;
    use super::SHUTDOWN_REQUESTED;
    use super::WORKER_QUEUE_CAPACITY;
    use image::DynamicImage;
    use image::ImageFormat as ImgFmt;
    use image::RgbImage;
    use std::ffi::OsStr;
    use std::io::Cursor;
    use std::io::Read;
    use std::io::Write;
    use std::net::Shutdown;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn make_secure_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create tempdir");
        std::fs::set_permissions(dir.path(), PermissionsExt::from_mode(0o700))
            .expect("restrict tempdir permissions");
        dir
    }

    fn make_test_jpeg_bytes(w: u32, h: u32) -> Vec<u8> {
        let img =
            DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |_, _| image::Rgb([180, 90, 45])));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImgFmt::Jpeg)
            .expect("encode test JPEG");
        buf
    }

    fn make_test_mp3_with_apic(img_bytes: &[u8]) -> Vec<u8> {
        let mut apic_payload = Vec::new();
        apic_payload.push(0); // UTF-8/ISO-8859-1 encoding
        apic_payload.extend_from_slice(b"image/jpeg\0");
        apic_payload.push(3); // Front Cover
        apic_payload.push(0); // empty description
        apic_payload.extend_from_slice(img_bytes);

        let frame_size = (apic_payload.len() as u32).to_be_bytes();
        let mut tag_body = Vec::new();
        tag_body.extend_from_slice(b"APIC");
        tag_body.extend_from_slice(&frame_size);
        tag_body.extend_from_slice(&[0, 0]);
        tag_body.extend_from_slice(&apic_payload);

        let sz = tag_body.len() as u32;
        let syncsafe = [
            ((sz >> 21) & 0x7f) as u8,
            ((sz >> 14) & 0x7f) as u8,
            ((sz >> 7) & 0x7f) as u8,
            (sz & 0x7f) as u8,
        ];
        let mut mp3_bytes = Vec::new();
        mp3_bytes.extend_from_slice(b"ID3\x03\x00\x00");
        mp3_bytes.extend_from_slice(&syncsafe);
        mp3_bytes.extend_from_slice(&tag_body);
        // Minimal MPEG1 Layer III frame header + padding so lofty parses cleanly.
        mp3_bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        mp3_bytes.resize(mp3_bytes.len() + 413, 0);
        mp3_bytes
    }

    fn wait_for_socket(sock_path: &Path) {
        let mut ready = false;
        for _ in 0..100 {
            if sock_path.exists() {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ready, "server socket did not appear");
    }

    fn spawn_server_and_wait(
        sock_path: &Path,
        faster: bool,
        requests: usize,
    ) -> std::thread::JoinHandle<()> {
        let server_sock = sock_path.to_path_buf();
        let handle = std::thread::spawn(move || {
            let local_shutdown = AtomicBool::new(false);
            run_server_with_shutdown(
                &server_sock,
                faster,
                Some(requests),
                false,
                &local_shutdown,
                false,
            )
            .expect("server loop failed");
        });
        wait_for_socket(sock_path);
        handle
    }

    fn send_frame(sock_path: &Path, frame: &[u8], shutdown_write: bool) -> String {
        let mut stream = UnixStream::connect(sock_path).expect("connect test socket");
        if !frame.is_empty() {
            stream.write_all(frame).expect("write test frame");
            stream.flush().expect("flush test frame");
        }
        if shutdown_write {
            stream
                .shutdown(Shutdown::Write)
                .expect("shutdown write half");
        }
        let mut resp = String::new();
        stream.read_to_string(&mut resp).expect("read response");
        resp
    }

    #[test]
    fn test_socket_malformed_frame_and_invalid_base64_recovery() {
        let dir = make_secure_tempdir();
        let sock_path = dir.path().join("sbs_artwork_test");
        let cacheroot = dir.path().to_str().expect("UTF-8 tempdir path");
        let handle = spawn_server_and_wait(&sock_path, false, 4);

        let invalid_b64_frame =
            format!("data\x00100x100_o\0{cacheroot}\0key1\0!!!NOT_BASE64!!!\0\r\n");
        let resp_b64 = send_frame(&sock_path, invalid_b64_frame.as_bytes(), false);
        assert!(
            resp_b64.starts_with("Error: invalid base64 data:"),
            "unexpected response: {resp_b64}"
        );

        let short_frame = b"data\x00100x100_o\0short\r\n";
        let resp_short = send_frame(&sock_path, short_frame, false);
        assert!(
            resp_short.starts_with("Error: malformed frame:"),
            "unexpected response: {resp_short}"
        );

        let unterminated = format!("data\x0040x40_o\0{cacheroot}\0key2\0abc");
        let resp_unterm = send_frame(&sock_path, unterminated.as_bytes(), true);
        assert!(
            resp_unterm.starts_with("Error: frame missing trailing newline"),
            "unexpected response: {resp_unterm}"
        );

        let jpg = make_test_jpeg_bytes(80, 80);
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &jpg);
        let valid_frame =
            format!("data\x0040x40_o\0{cacheroot}\0music/1/cover_40x40_o\0{b64}\0\r\n");
        let resp_ok = send_frame(&sock_path, valid_frame.as_bytes(), false);
        assert_eq!(resp_ok, "OK\r\n");

        handle.join().expect("join server thread");
    }

    #[test]
    fn test_socket_empty_frame_and_missing_params() {
        let dir = make_secure_tempdir();
        let sock_path = dir.path().join("sbs_artwork_empty_test");
        let handle = spawn_server_and_wait(&sock_path, false, 2);

        let resp_empty = send_frame(&sock_path, b"", true);
        assert_eq!(resp_empty, "Error: empty frame\r\n");

        let resp_missing = send_frame(&sock_path, b"\0\0\0\0\0\r\n", false);
        assert_eq!(resp_missing, "Error: missing required parameters\r\n");

        handle.join().expect("join server thread");
    }

    #[test]
    fn test_socket_owner_only_permissions() {
        let dir = make_secure_tempdir();
        let sock_path = dir.path().join("sbs_artwork_perm_test");
        let cacheroot = dir.path().to_str().expect("UTF-8 tempdir path");
        let handle = spawn_server_and_wait(&sock_path, false, 1);

        let meta = std::fs::symlink_metadata(&sock_path).expect("stat socket");
        let is_sock = meta.file_type().is_socket();
        let mode = meta.permissions().mode() & 0o777;

        assert!(is_sock, "socket path must be a unix socket");
        assert_eq!(mode, 0o600, "socket permissions must be owner-only 0o600");
        assert_eq!(mode & 0o077, 0, "group/other bits must be zero");

        let jpg = make_test_jpeg_bytes(40, 40);
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &jpg);
        let valid_frame = format!("data\x0020x20_o\0{cacheroot}\0music/perm/cover\0{b64}\0\r\n");
        let resp = send_frame(&sock_path, valid_frame.as_bytes(), false);
        assert_eq!(resp, "OK\r\n");

        handle.join().expect("join server thread");
    }

    #[test]
    fn test_socket_rejects_preexisting_symlink() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let target = dir.path().join("preserved_target");
        std::fs::write(&target, b"keep_me").expect("write target file");
        let sock_path = dir.path().join("sbs_symlink");
        std::os::unix::fs::symlink(&target, &sock_path).expect("create symlink");

        let res = run_server_n_requests(&sock_path, false, 1);
        assert!(res.is_err(), "server must reject pre-existing symlink");

        let link_meta = std::fs::symlink_metadata(&sock_path).expect("symlink preserved");
        assert!(link_meta.file_type().is_symlink());
        let content = std::fs::read(&target).expect("read target file");
        assert_eq!(content, b"keep_me");
    }

    #[test]
    fn test_socket_slow_incomplete_frame_times_out() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sock_path = dir.path().join("sbs_artwork_timeout_test");
        let handle = spawn_server_and_wait(&sock_path, false, 1);

        let mut stream = UnixStream::connect(&sock_path).expect("connect test socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("set client read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("set client write timeout");
        stream
            .write_all(b"partial_frame_without_newline")
            .expect("write partial frame");
        stream.flush().expect("flush partial frame");

        // Hold write half open so server hits SOCKET_IO_TIMEOUT cleanly.
        let mut resp = String::new();
        stream
            .read_to_string(&mut resp)
            .expect("read timeout error response");
        assert!(resp.starts_with("Error: "), "unexpected response: {resp}");
        assert!(
            resp.contains("timed out") || resp.contains("deadline"),
            "expected timeout error, got: {resp}"
        );

        handle.join().expect("join server thread");
    }

    #[test]
    fn test_rejects_non_regular_file_directory_and_fifo() {
        let dir = make_secure_tempdir();
        let subdir = dir.path().join("input_dir");
        std::fs::create_dir(&subdir).expect("create subdir");

        let fifo_path = dir.path().join("input.fifo");
        let status = Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .expect("invoke mkfifo");
        assert!(status.success(), "mkfifo failed");

        let cli_dir_err = read_cli_input_image(&subdir).expect_err("directory must fail");
        assert!(cli_dir_err.contains("not a regular file"));

        let cli_fifo_err = read_cli_input_image(&fifo_path).expect_err("FIFO must fail");
        assert!(cli_fifo_err.contains("not a regular file"));

        let sock_path = dir.path().join("sbs_artwork_nonreg_test");
        let cacheroot = dir.path().to_str().expect("UTF-8 tempdir path");
        let handle = spawn_server_and_wait(&sock_path, false, 2);

        let dir_str = subdir.to_str().expect("UTF-8 subdir path");
        let dir_frame = format!("{dir_str}\x0050x50_m\0{cacheroot}\0music/dir/cover\0\0\r\n");
        let dir_resp = send_frame(&sock_path, dir_frame.as_bytes(), false);
        assert!(
            dir_resp.starts_with("Error: input path is not a regular file:"),
            "unexpected dir response: {dir_resp}"
        );

        let fifo_str = fifo_path.to_str().expect("UTF-8 fifo path");
        let fifo_frame = format!("{fifo_str}\x0050x50_m\0{cacheroot}\0music/fifo/cover\0\0\r\n");
        let fifo_resp = send_frame(&sock_path, fifo_frame.as_bytes(), false);
        assert!(
            fifo_resp.starts_with("Error: input path is not a regular file:"),
            "unexpected fifo response: {fifo_resp}"
        );

        handle.join().expect("join server thread");
    }

    #[test]
    fn test_cli_resize_one_shot() {
        let jpg = make_test_jpeg_bytes(80, 80);
        let (fmt, data) = resize_one_shot(&jpg, "40x40_o", false).expect("resize one shot");
        assert_eq!(fmt, "jpg");
        assert!(!data.is_empty());
    }

    #[test]
    fn test_cli_resize_multi_spec_and_stdin_tag_extraction_parity() {
        let jpg = make_test_jpeg_bytes(80, 80);
        let (fmt_multi, data_multi) =
            resize_one_shot(&jpg, "40x40_o,20x20_m", false).expect("multi-spec resize");
        assert_eq!(fmt_multi, "jpg");
        let decoded = image::load_from_memory(&data_multi).expect("decode multi-spec output");
        assert_eq!(decoded.width(), 40);
        assert_eq!(decoded.height(), 40);

        let (fmt_leading_comma, _) =
            resize_one_shot(&jpg, ",40x40_o,20x20_m,", false).expect("comma-padded spec");
        assert_eq!(fmt_leading_comma, "jpg");

        let (fmt_empty, _) = resize_one_shot(&jpg, ",,,", false).expect("empty specs fallback");
        assert_eq!(fmt_empty, "jpg");

        let mp3_bytes = make_test_mp3_with_apic(&jpg);
        let extracted = extract_image_from_bytes(&mp3_bytes).expect("extract ID3 APIC from bytes");
        assert_eq!(extracted.as_ref(), jpg.as_slice());

        let direct = extract_image_from_bytes(&jpg).expect("direct image passthrough");
        assert_eq!(direct.as_ref(), jpg.as_slice());
    }

    #[test]
    fn test_parse_cli_args_valid_and_invalid() {
        let daemon_args = vec![
            "gdresized",
            "--socket",
            "/run/gdresized-test.sock",
            "--faster",
            "--max-requests",
            "42",
            "--debug",
        ];
        let opts = parse_cli_args(daemon_args).expect("parse valid daemon args");
        assert_eq!(
            opts.socket_path.as_deref(),
            Some(Path::new("/run/gdresized-test.sock"))
        );
        assert!(opts.faster);
        assert!(opts.debug);
        assert_eq!(opts.max_requests, Some(42));

        let file_cli_args = vec![
            "gdresized",
            "--cli-resize",
            "--spec",
            "100x100_o",
            "--file",
            "/run/cover.jpg",
            "--help",
        ];
        let file_opts = parse_cli_args(file_cli_args).expect("parse valid file cli-resize args");
        assert!(file_opts.cli_resize);
        assert_eq!(file_opts.spec.as_deref(), Some("100x100_o"));
        assert_eq!(file_opts.file.as_deref(), Some(Path::new("/run/cover.jpg")));
        assert!(!file_opts.stdin);
        assert!(file_opts.help);

        let stdin_cli_args = vec![
            "gdresized",
            "--cli-resize",
            "--spec",
            "100x100_o",
            "--stdin",
        ];
        let stdin_opts = parse_cli_args(stdin_cli_args).expect("parse valid stdin cli-resize args");
        assert!(stdin_opts.cli_resize);
        assert!(stdin_opts.stdin);
        assert!(stdin_opts.file.is_none());

        let invalid_cases = [
            vec!["gdresized", "--socket"],
            vec!["gdresized", "--socket", ""],
            vec!["gdresized", "--max-requests"],
            vec!["gdresized", "--max-requests", "not_a_number"],
            vec!["gdresized", "--spec"],
            vec!["gdresized", "--file"],
            vec!["gdresized", "--file", ""],
            vec!["gdresized", "--file", "/run/cover.jpg", "--stdin"],
            vec!["gdresized", "-d_startup"],
            vec!["gdresized", "--unknown-flag"],
        ];
        for case in invalid_cases {
            assert!(
                parse_cli_args(case.clone()).is_err(),
                "expected parse error for {case:?}"
            );
        }
    }

    #[test]
    fn test_socket_file_input_and_artwork_store_verification() {
        let dir = make_secure_tempdir();
        let sock_path = dir.path().join("sbs_artwork_file_test");

        let jpg_path = dir.path().join("test_cover.jpg");
        let jpg_bytes = make_test_jpeg_bytes(100, 100);
        std::fs::write(&jpg_path, &jpg_bytes).expect("write test jpeg");

        let handle = spawn_server_and_wait(&sock_path, false, 1);

        let cacheroot = dir.path().to_str().expect("UTF-8 tempdir path");
        let jpg_str = jpg_path.to_str().expect("UTF-8 jpeg path");
        let frame = format!("{jpg_str}\x0050x50_m\0{cacheroot}\0music/99/cover\0\0\r\n");
        let resp = send_frame(&sock_path, frame.as_bytes(), false);
        assert_eq!(resp, "OK\r\n");
        handle.join().expect("join server thread");

        // Verify SQLite store contains the resized image with original_path & mtime.
        let store = lms_cache::DbArtworkStore::open(dir.path(), false).expect("open artwork store");
        let record = store
            .get("music/99/cover")
            .expect("query artwork store")
            .expect("record must exist");
        assert_eq!(record.content_type, "jpg");
        assert_eq!(record.original_path, jpg_str);
        assert!(record.mtime > 0, "expected non-zero mtime in artwork.db");
        assert!(!record.data.is_empty());
    }

    #[test]
    fn test_current_euid_and_hardened_cacheroot_validation() {
        let proc_meta = std::fs::metadata("/proc/self").expect("stat /proc/self");
        assert_eq!(
            super::current_euid().expect("geteuid"),
            std::os::unix::fs::MetadataExt::uid(&proc_meta)
        );

        assert!(
            super::validate_cacheroot(Path::new("relative/cache")).is_err(),
            "relative cacheroot must be rejected"
        );

        let dir = tempfile::tempdir().expect("create tempdir");
        let real_dir = dir.path().join("real_cache");
        std::fs::create_dir(&real_dir).expect("create real_cache");
        std::fs::set_permissions(&real_dir, PermissionsExt::from_mode(0o755))
            .expect("set 0o755 mode");
        assert!(
            super::validate_cacheroot(&real_dir).is_ok(),
            "owner-only writable absolute dir must succeed"
        );

        let symlink_dir = dir.path().join("link_cache");
        std::os::unix::fs::symlink(&real_dir, &symlink_dir).expect("create symlink");
        assert!(
            super::validate_cacheroot(&symlink_dir).is_err(),
            "symlink cacheroot must be rejected"
        );

        std::fs::set_permissions(&real_dir, PermissionsExt::from_mode(0o775))
            .expect("set 0o775 mode");
        assert!(
            super::validate_cacheroot(&real_dir).is_err(),
            "group-writable cacheroot must be rejected"
        );
    }

    #[test]
    fn test_max_requests_zero_and_live_socket_preservation() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sock_path = dir.path().join("sbs_live_test");
        let _listener =
            std::os::unix::net::UnixListener::bind(&sock_path).expect("bind active listener");

        super::run_server(&sock_path, false, Some(0))
            .expect("max_requests=0 must succeed without touching socket");
        assert!(
            sock_path.exists(),
            "max_requests=0 must not unlink existing socket"
        );

        let err = super::run_server(&sock_path, false, Some(1))
            .expect_err("starting server on live peer socket must fail");
        assert!(
            err.to_string().contains("live peer"),
            "unexpected error: {err}"
        );
        assert!(
            sock_path.exists(),
            "live peer socket must not be unlinked on error"
        );
    }

    #[test]
    fn test_single_open_rejects_oversized_and_extracts_id3_apic() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let big_path = dir.path().join("oversized.bin");
        let big_file = std::fs::File::create(&big_path).expect("create sparse file");
        big_file
            .set_len(super::MAX_INPUT_FILE_BYTES + 1)
            .expect("set sparse length");
        let err = read_cli_input_image(&big_path).expect_err("oversized file must fail");
        assert!(
            err.contains("exceeds 64 MiB limit"),
            "unexpected error: {err}"
        );

        let jpg = make_test_jpeg_bytes(40, 40);
        let mp3_bytes = make_test_mp3_with_apic(&jpg);
        let mp3_path = dir.path().join("track.mp3");
        std::fs::write(&mp3_path, &mp3_bytes).expect("write test mp3");
        let extracted = read_cli_input_image(&mp3_path).expect("extract ID3 APIC");
        assert_eq!(extracted, jpg);
    }

    #[test]
    fn test_rejects_relative_input_paths_and_input_symlinks() {
        let dir = make_secure_tempdir();
        let real_jpg = dir.path().join("real_cover.jpg");
        std::fs::write(&real_jpg, make_test_jpeg_bytes(40, 40)).expect("write real jpg");

        let link_jpg = dir.path().join("link_cover.jpg");
        std::os::unix::fs::symlink(&real_jpg, &link_jpg).expect("create file symlink");

        let sub_real = dir.path().join("real_sub");
        std::fs::create_dir(&sub_real).expect("create real_sub");
        let sub_jpg = sub_real.join("cover.jpg");
        std::fs::write(&sub_jpg, make_test_jpeg_bytes(40, 40)).expect("write sub jpg");
        let sub_link = dir.path().join("link_sub");
        std::os::unix::fs::symlink(&sub_real, &sub_link).expect("create dir symlink");
        let traversed_jpg = sub_link.join("cover.jpg");

        let rel_err =
            read_cli_input_image(Path::new("relative/cover.jpg")).expect_err("relative must fail");
        assert!(
            rel_err.contains("must be absolute"),
            "unexpected: {rel_err}"
        );

        let dotdot_path = dir.path().join("sub/../real_cover.jpg");
        let dotdot_err = read_cli_input_image(&dotdot_path).expect_err("dotdot must fail");
        assert!(
            dotdot_err.contains("'.' or '..'"),
            "unexpected: {dotdot_err}"
        );

        let sym_err = read_cli_input_image(&link_jpg).expect_err("file symlink must fail");
        assert!(sym_err.contains("symlink"), "unexpected: {sym_err}");

        let trav_err = read_cli_input_image(&traversed_jpg).expect_err("dir symlink must fail");
        assert!(trav_err.contains("symlink"), "unexpected: {trav_err}");

        let sock_path = dir.path().join("sbs_input_sec.sock");
        let cacheroot = dir.path().to_str().expect("UTF-8 cacheroot");
        let handle = spawn_server_and_wait(&sock_path, false, 2);

        let rel_frame = format!("relative/cover.jpg\x0040x40_o\0{cacheroot}\0k1\0\0\r\n");
        let rel_resp = send_frame(&sock_path, rel_frame.as_bytes(), false);
        assert!(
            rel_resp.starts_with("Error: input path must be absolute:"),
            "unexpected rel_resp: {rel_resp}"
        );

        let sym_str = link_jpg.to_str().expect("UTF-8 symlink path");
        let sym_frame = format!("{sym_str}\x0040x40_o\0{cacheroot}\0k2\0\0\r\n");
        let sym_resp = send_frame(&sock_path, sym_frame.as_bytes(), false);
        assert!(
            sym_resp.starts_with("Error: input path must not traverse symlinks:"),
            "unexpected sym_resp: {sym_resp}"
        );

        handle.join().expect("join server thread");
    }

    #[test]
    fn test_ipc_frame_strict_5th_nul_and_non_utf8_paths() {
        let dir = make_secure_tempdir();
        let sock_path = dir.path().join("sbs_protocol_test.sock");
        let cacheroot = dir.path().to_str().expect("UTF-8 cacheroot");
        let handle = spawn_server_and_wait(&sock_path, false, 3);

        let extra_nul_frame = format!("data\x0040x40_o\0{cacheroot}\0k1\0abc\0\0\r\n");
        let resp_extra_nul = send_frame(&sock_path, extra_nul_frame.as_bytes(), false);
        assert!(
            resp_extra_nul.starts_with("Error: malformed frame: unexpected trailing"),
            "unexpected response: {resp_extra_nul}"
        );

        let trailing_bytes_frame = format!("data\x0040x40_o\0{cacheroot}\0k2\0abc\0extra\r\n");
        let resp_trailing = send_frame(&sock_path, trailing_bytes_frame.as_bytes(), false);
        assert!(
            resp_trailing.starts_with("Error: malformed frame: unexpected trailing"),
            "unexpected response: {resp_trailing}"
        );

        let non_utf8_name = OsStr::from_bytes(b"cover_\xff\xfe.jpg");
        let non_utf8_path = dir.path().join(non_utf8_name);
        std::fs::write(&non_utf8_path, make_test_jpeg_bytes(60, 60)).expect("write non-UTF-8 jpg");

        let mut frame = Vec::new();
        frame.extend_from_slice(non_utf8_path.as_os_str().as_bytes());
        frame.extend_from_slice(b"\x0030x30_o\0");
        frame.extend_from_slice(cacheroot.as_bytes());
        frame.extend_from_slice(b"\0music/nonutf8/cover\0\0\r\n");

        let resp_non_utf8 = send_frame(&sock_path, &frame, false);
        assert_eq!(resp_non_utf8, "OK\r\n");

        handle.join().expect("join server thread");
    }

    #[test]
    fn test_strict_base64_and_corrupt_inputs() {
        assert!(resize_one_shot(b"not an image", "40x40_o", false).is_err());
        assert!(extract_image_from_bytes(b"").is_err());
        assert!(extract_image_from_bytes(b"ID3\x03\x00\x00\x00\x00\x00\x04APIC").is_err());

        let dir = make_secure_tempdir();
        let sock_path = dir.path().join("sbs_corrupt_test.sock");
        let cacheroot = dir.path().to_str().expect("UTF-8 cacheroot");
        let handle = spawn_server_and_wait(&sock_path, false, 4);

        // Unpadded base64 must be rejected strictly.
        let unpadded = format!("data\x0040x40_o\0{cacheroot}\0k1\0AQI\0\r\n");
        let resp_unpadded = send_frame(&sock_path, unpadded.as_bytes(), false);
        assert!(
            resp_unpadded.starts_with("Error: invalid base64 data:"),
            "unexpected: {resp_unpadded}"
        );

        // Non-canonical trailing bits must be rejected strictly.
        let non_canon = format!("data\x0040x40_o\0{cacheroot}\0k2\0AR==\0\r\n");
        let resp_non_canon = send_frame(&sock_path, non_canon.as_bytes(), false);
        assert!(
            resp_non_canon.starts_with("Error: invalid base64 data:"),
            "unexpected: {resp_non_canon}"
        );

        // Invalid padding only must be rejected strictly.
        let empty_pad = format!("data\x0040x40_o\0{cacheroot}\0k3\0====\0\r\n");
        let resp_pad = send_frame(&sock_path, empty_pad.as_bytes(), false);
        assert!(
            resp_pad.starts_with("Error: invalid base64 data:"),
            "unexpected: {resp_pad}"
        );

        // Corrupt image with valid base64 must return Error without crashing.
        let corrupt_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            b"\xff\xd8\xff\x00",
        );
        let corrupt_frame = format!("data\x0040x40_o\0{cacheroot}\0k4\0{corrupt_b64}\0\r\n");
        let resp_corrupt = send_frame(&sock_path, corrupt_frame.as_bytes(), false);
        assert!(
            resp_corrupt.starts_with("Error: "),
            "unexpected: {resp_corrupt}"
        );

        handle.join().expect("join server thread");
    }

    #[test]
    fn test_worker_pool_processes_requests_and_backpressure() {
        let dir = make_secure_tempdir();
        let cacheroot = dir.path().to_str().expect("UTF-8 cacheroot");
        let jpg = make_test_jpeg_bytes(48, 48);
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &jpg);

        let mut pool = WorkerPool::spawn(false, false).expect("spawn worker pool");
        let mut client_readers = Vec::new();
        for idx in 0..4 {
            let (client_stream, server_stream) = UnixStream::pair().expect("unix stream pair");
            let frame =
                format!("data\x0024x24_o\0{cacheroot}\0music/wp/{idx}\0{b64}\0\r\n").into_bytes();
            let req = ClientRequest::new(
                server_stream,
                Some(frame),
                Instant::now() + Duration::from_secs(5),
            );
            assert!(dispatch_ready_request(req, Some(&pool), false, false));
            client_stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set read timeout");
            client_readers.push(client_stream);
        }

        pool.shutdown().expect("worker pool shutdown");
        for mut reader in client_readers {
            let mut resp = String::new();
            reader
                .read_to_string(&mut resp)
                .expect("read worker response");
            assert_eq!(resp, "OK\r\n");
        }

        // Verify queue capacity and closed state backpressure deterministically.
        let queue = WorkerQueue::new();
        let mut kept_peers = Vec::new();
        for _ in 0..WORKER_QUEUE_CAPACITY {
            let (peer, server_stream) = UnixStream::pair().expect("unix stream pair");
            kept_peers.push(peer);
            let req = ClientRequest::new(
                server_stream,
                Some(Vec::new()),
                Instant::now() + Duration::from_secs(5),
            );
            assert!(queue.try_enqueue(req).is_ok());
        }
        let (_overflow_peer, overflow_stream) = UnixStream::pair().expect("unix stream pair");
        let overflow_req = ClientRequest::new(
            overflow_stream,
            Some(Vec::new()),
            Instant::now() + Duration::from_secs(5),
        );
        assert!(matches!(
            queue.try_enqueue(overflow_req),
            Err(EnqueueError::Full(_))
        ));

        queue.close().expect("close queue");
        let (_closed_peer, closed_stream) = UnixStream::pair().expect("unix stream pair");
        let closed_req = ClientRequest::new(
            closed_stream,
            Some(Vec::new()),
            Instant::now() + Duration::from_secs(5),
        );
        assert!(matches!(
            queue.try_enqueue(closed_req),
            Err(EnqueueError::Closed(_))
        ));
    }

    #[test]
    fn test_worker_panic_unwind_sends_error_and_recovers_pool() {
        fn trigger_worker_panic() {
            panic!("simulated worker panic for CONC-008");
        }

        let dir = make_secure_tempdir();
        let cacheroot = dir.path().to_str().expect("UTF-8 cacheroot");
        let jpg = make_test_jpeg_bytes(48, 48);
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &jpg);

        let mut pool = WorkerPool::spawn(false, false).expect("spawn worker pool");

        // Crash all WORKER_COUNT workers via simulated panic unwinds and verify
        // RequestReplyGuard delivers the error response on every panicked request.
        let mut panic_readers = Vec::new();
        for _ in 0..super::WORKER_COUNT {
            let (client_stream, server_stream) = UnixStream::pair().expect("unix stream pair");
            client_stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set read timeout");
            let req = ClientRequest::with_panic_hook(
                server_stream,
                Some(Vec::new()),
                Instant::now() + Duration::from_secs(5),
                trigger_worker_panic,
            );
            assert!(dispatch_ready_request(req, Some(&pool), false, false));
            panic_readers.push(client_stream);
        }

        for mut reader in panic_readers {
            let mut resp = String::new();
            reader
                .read_to_string(&mut resp)
                .expect("read panic guard response");
            assert_eq!(resp, "Error: internal worker error\r\n");
        }

        // Verify WorkerSentinel respawned workers that still service requests.
        let mut recovery_readers = Vec::new();
        for idx in 0..super::WORKER_COUNT {
            let (client_stream, server_stream) = UnixStream::pair().expect("unix stream pair");
            client_stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set read timeout");
            let frame = format!("data\x0024x24_o\0{cacheroot}\0music/recover/{idx}\0{b64}\0\r\n")
                .into_bytes();
            let req = ClientRequest::new(
                server_stream,
                Some(frame),
                Instant::now() + Duration::from_secs(5),
            );
            assert!(dispatch_ready_request(req, Some(&pool), false, false));
            recovery_readers.push(client_stream);
        }

        for mut reader in recovery_readers {
            let mut resp = String::new();
            reader
                .read_to_string(&mut resp)
                .expect("read recovered worker response");
            assert_eq!(resp, "OK\r\n");
        }

        let wait_deadline = Instant::now() + Duration::from_secs(2);
        while pool.restart_count() < super::WORKER_COUNT && Instant::now() < wait_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(pool.restart_count(), super::WORKER_COUNT);
        pool.shutdown().expect("recovered pool shutdown");
    }

    #[test]
    fn test_bind_owner_only_socket_stale_replace_and_noreplace() {
        let dir = make_secure_tempdir();
        let sock_path = dir.path().join("sbs_stale_replace.sock");

        // Create a dead socket file (listener dropped without unlinking).
        {
            let _dead_listener = UnixListener::bind(&sock_path).expect("bind initial dead socket");
        }
        assert!(sock_path.exists());

        // Atomically replace the dead socket without unlinking first.
        let (listener, guard) =
            super::bind_owner_only_socket(&sock_path).expect("replace stale dead socket");
        let meta = std::fs::symlink_metadata(&sock_path).expect("stat replaced socket");
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        drop(listener);
        drop(guard);
        assert!(!sock_path.exists());

        // rename_noreplace must refuse to overwrite an existing entry.
        let src = dir.path().join("staged_file");
        let dst = dir.path().join("existing_file");
        std::fs::write(&src, b"new").expect("write src");
        std::fs::write(&dst, b"keep").expect("write dst");
        let err = super::rename_noreplace(&src, &dst).expect_err("noreplace must fail on existing");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&dst).expect("read dst"), b"keep");
    }

    #[test]
    fn test_signal_shutdown_flag_and_server_exit() {
        struct ResetShutdownFlag;
        impl Drop for ResetShutdownFlag {
            fn drop(&mut self) {
                SHUTDOWN_REQUESTED.store(false, Ordering::Release);
            }
        }
        let _reset = ResetShutdownFlag;
        SHUTDOWN_REQUESTED.store(false, Ordering::Release);
        handle_shutdown_signal(libc::SIGTERM);
        assert!(SHUTDOWN_REQUESTED.load(Ordering::Acquire));

        let local_flag = Arc::new(AtomicBool::new(false));
        let ppid = std::os::unix::process::parent_id();
        assert!(!should_exit_server_with_flag(ppid, &local_flag));

        let dir = make_secure_tempdir();
        let sock_path = dir.path().join("sbs_shutdown_test.sock");
        let thread_sock = sock_path.clone();
        let thread_flag = Arc::clone(&local_flag);
        let handle = std::thread::spawn(move || {
            run_server_with_shutdown(&thread_sock, false, None, false, &thread_flag, false)
                .expect("server shutdown cleanly");
        });
        wait_for_socket(&sock_path);

        local_flag.store(true, Ordering::Release);
        assert!(should_exit_server_with_flag(ppid, &local_flag));
        handle.join().expect("join shutdown server thread");
        assert!(
            !sock_path.exists(),
            "SocketGuard must remove socket on shutdown"
        );
    }

    #[test]
    fn test_socket_guard_cleanup_and_inode_preservation() {
        let dir = make_secure_tempdir();

        // Matching socket is removed on drop.
        let sock1 = dir.path().join("guard_remove.sock");
        let listener1 = UnixListener::bind(&sock1).expect("bind sock1");
        let meta1 = std::fs::symlink_metadata(&sock1).expect("stat sock1");
        let guard1 = SocketGuard::new(sock1.clone(), meta1.dev(), meta1.ino());
        drop(listener1);
        drop(guard1);
        assert!(!sock1.exists(), "matching socket must be removed on drop");

        // Replaced socket with different inode is preserved on drop.
        let sock2 = dir.path().join("guard_mismatch.sock");
        let listener2 = UnixListener::bind(&sock2).expect("bind sock2");
        let meta2 = std::fs::symlink_metadata(&sock2).expect("stat sock2");
        let guard2 = SocketGuard::new(sock2.clone(), meta2.dev(), meta2.ino().wrapping_add(1));
        drop(guard2);
        assert!(sock2.exists(), "mismatched inode socket must be preserved");
        drop(listener2);

        // Non-socket regular file is never removed on drop.
        let reg = dir.path().join("not_a_socket");
        std::fs::write(&reg, b"keep").expect("write reg file");
        let meta_reg = std::fs::symlink_metadata(&reg).expect("stat reg file");
        let guard_reg = SocketGuard::new(reg.clone(), meta_reg.dev(), meta_reg.ino());
        drop(guard_reg);
        assert!(reg.exists(), "non-socket file must not be removed");
    }

    #[test]
    fn test_store_cache_eviction_on_sqlite_error() {
        let dir = make_secure_tempdir();
        let store1 = open_or_cached_store(dir.path(), false).expect("open store");
        let store2 = open_or_cached_store(dir.path(), false).expect("cached store hit");
        assert!(Arc::ptr_eq(&store1, &store2));

        // Corrupt all database files on disk so SQLite fails on the cached store.
        let mut touched_files = Vec::new();
        for entry in std::fs::read_dir(dir.path()).expect("read cacheroot dir") {
            let entry = entry.expect("dir entry");
            if entry.file_type().expect("file type").is_file() {
                std::fs::write(entry.path(), vec![0x5a_u8; 4096]).expect("corrupt db file");
                touched_files.push(entry.path());
            }
        }
        assert!(
            !touched_files.is_empty(),
            "expected sqlite files in cacheroot"
        );

        let sock_path = dir.path().join("sbs_cache_evict.sock");
        let cacheroot = dir.path().to_str().expect("UTF-8 cacheroot");
        let handle = spawn_server_and_wait(&sock_path, false, 1);

        let jpg = make_test_jpeg_bytes(40, 40);
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &jpg);
        let frame = format!("data\x0020x20_o\0{cacheroot}\0music/corrupt/cover\0{b64}\0\r\n");
        let resp = send_frame(&sock_path, frame.as_bytes(), false);
        assert!(resp.starts_with("Error: "), "expected SQLite error: {resp}");
        handle.join().expect("join server thread");

        // Remove corrupted DB files and verify store cache returns a fresh instance.
        for path in touched_files {
            let _ = std::fs::remove_file(path);
        }
        let store3 = open_or_cached_store(dir.path(), false).expect("reopen fresh store");
        assert!(!Arc::ptr_eq(&store1, &store3));
        evict_cached_store(dir.path(), false);
    }

    #[test]
    fn test_sanitize_for_log_scrubs_ascii_control_chars() {
        let dirty = "path\nwith\r\x1b[31mred\x00and\x7fctrl";
        let clean = sanitize_for_log(dirty);
        assert!(!clean.chars().any(|c| c.is_ascii_control()));
        assert_eq!(clean, "path\\x0awith\\x0d\\x1b[31mred\\x00and\\x7fctrl");
    }
}
