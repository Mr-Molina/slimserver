#![deny(unsafe_op_in_unsafe_fn)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Once;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

const DEFAULT_SOCKET_PATH: &str = "/tmp/sbs_artwork";
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
const MAX_INPUT_FILE_BYTES: u64 = 64 * 1024 * 1024;
const SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(15);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_STORE_CACHE_ENTRIES: usize = 8;
const WORKER_COUNT: usize = 4;
const WORKER_QUEUE_CAPACITY: usize = 64;

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static SIGNAL_HANDLER_INIT: Once = Once::new();

extern "C" fn handle_shutdown_signal(_sig: std::ffi::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::Release);
}

fn install_signal_handlers() {
    SIGNAL_HANDLER_INIT.call_once(|| {
        const SIGINT: std::ffi::c_int = 2;
        const SIGTERM: std::ffi::c_int = 15;

        extern "C" {
            fn signal(sig: std::ffi::c_int, handler: extern "C" fn(std::ffi::c_int)) -> usize;
        }

        // JUSTIFICATION: Register async-signal-safe SIGINT/SIGTERM handlers
        // via POSIX signal(2) without adding Cargo dependencies.
        // SAFETY:
        // 1. Preconditions: Called at most once via Once during init.
        // 2. Bounds/Alignment: Integer signal IDs and valid fn pointer.
        // 3. Aliasing/Lifetime: Handler fn has static program lifetime.
        // 4. Async-signal safety: Handler only executes AtomicBool::store.
        // 5. Return value: Previous disposition pointer is discarded.
        // 6. Failure mode: On SIG_ERR, ppid/EOF checks still exit loop.
        unsafe {
            let _ = signal(SIGINT, handle_shutdown_signal);
            let _ = signal(SIGTERM, handle_shutdown_signal);
        }
    });
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
    Ok(opts)
}

#[cfg(unix)]
mod unix_ffi {
    // JUSTIFICATION: Query effective UID and serialize socket umask via POSIX
    // libc without adding external crate dependencies or procfs reads.
    // SAFETY:
    // 1. Preconditions: Linked against standard POSIX libc on Unix targets.
    // 2. Bounds/Alignment: Signatures pass and return scalar u32 values only.
    // 3. Aliasing/Lifetime: No pointers or borrows cross the FFI boundary.
    // 4. Thread safety: geteuid is thread-safe; umask uses UMASK_LOCK.
    // 5. Return value: u32 matches uid_t/mode_t on supported Unix targets.
    // 6. Failure mode: POSIX geteuid(2) and umask(2) are infallible.
    unsafe extern "C" {
        pub(super) fn geteuid() -> u32;
        pub(super) fn umask(mask: u32) -> u32;
    }
}

#[cfg(unix)]
static UMASK_LOCK: Mutex<()> = Mutex::new(());

#[cfg(unix)]
struct UmaskGuard<'a> {
    _lock: std::sync::MutexGuard<'a, ()>,
    prev_mask: u32,
}

#[cfg(unix)]
impl<'a> UmaskGuard<'a> {
    fn restrict_to_owner_rw() -> std::io::Result<Self> {
        let lock = UMASK_LOCK
            .lock()
            .map_err(|_| std::io::Error::other("umask mutex poisoned"))?;
        // JUSTIFICATION: Set process umask to 0o177 prior to UnixListener::bind
        // so the socket inode is created owner-only without a chmod race.
        // SAFETY:
        // 1. Preconditions: UMASK_LOCK serializes all process umask mutations.
        // 2. Bounds/Alignment: Passes scalar 0o177 u32 bitmask argument.
        // 3. Aliasing/Lifetime: No pointers or references cross FFI boundary.
        // 4. Thread safety: MutexGuard ensures exclusive access across threads.
        // 5. Return value: Prior u32 umask is saved and restored in Drop.
        // 6. Failure mode: POSIX umask(2) never fails; Drop always restores.
        let prev_mask = unsafe { unix_ffi::umask(0o177) };
        Ok(Self {
            _lock: lock,
            prev_mask,
        })
    }
}

#[cfg(unix)]
impl Drop for UmaskGuard<'_> {
    fn drop(&mut self) {
        // JUSTIFICATION: Restore original process umask after bind completes.
        // SAFETY:
        // 1. Preconditions: UMASK_LOCK remains held until Drop finishes.
        // 2. Bounds/Alignment: Passes saved scalar prev_mask u32 bitmask.
        // 3. Aliasing/Lifetime: No pointers or references cross FFI boundary.
        // 4. Thread safety: MutexGuard prevents concurrent umask changes.
        // 5. Return value: Intermediate mask return value is discarded.
        // 6. Failure mode: POSIX umask(2) is infallible and restores mask.
        unsafe {
            let _ = unix_ffi::umask(self.prev_mask);
        }
    }
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
        // 5. Return value: Always returns a valid u32 effective user ID.
        // 6. Failure mode: POSIX geteuid(2) is documented as infallible.
        Ok(unsafe { unix_ffi::geteuid() })
    }
    #[cfg(not(unix))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "geteuid is only supported on Unix",
        ))
    }
}

fn validate_cacheroot(cacheroot: &Path) -> std::io::Result<PathBuf> {
    if cacheroot.as_os_str().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cacheroot must not be empty",
        ));
    }
    for comp in cacheroot.components() {
        if matches!(comp, Component::ParentDir) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cacheroot must not contain '..' components",
            ));
        }
    }
    let canonical = std::fs::canonicalize(cacheroot)?;
    let meta = std::fs::metadata(&canonical)?;
    if !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cacheroot is not a directory",
        ));
    }
    let euid = current_euid()?;
    if meta.uid() != euid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cacheroot ownership does not match current user",
        ));
    }
    Ok(canonical)
}

struct SocketGuard {
    path: PathBuf,
}

impl SocketGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::symlink_metadata(&self.path) {
            if meta.file_type().is_socket() {
                if let Ok(euid) = current_euid() {
                    if meta.uid() == euid {
                        let _ = std::fs::remove_file(&self.path);
                    }
                }
            }
        }
    }
}

fn prepare_socket_path(sock_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    match std::fs::symlink_metadata(sock_path) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() || !ft.is_socket() {
                return Err(format!(
                    "refusing to remove non-socket path: {}",
                    sock_path.display()
                )
                .into());
            }
            let euid = current_euid()?;
            if meta.uid() != euid {
                return Err(format!(
                    "refusing to remove socket owned by uid {}: {}",
                    meta.uid(),
                    sock_path.display()
                )
                .into());
            }
            std::fs::remove_file(sock_path)?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Box::new(e)),
    }
}

pub fn resize_one_shot(
    input: &[u8],
    spec: &str,
    faster: bool,
) -> Result<(String, Vec<u8>), Box<dyn std::error::Error>> {
    let parsed_spec = lms_image::parse_single_spec(spec);
    let (bytes, format) = lms_image::resize_image(input, &parsed_spec, faster)?;
    Ok((format.as_str().to_string(), bytes))
}

type StoreCacheKey = (PathBuf, bool);
type StoreCacheMap = HashMap<StoreCacheKey, Arc<lms_cache::DbArtworkStore>>;

static STORE_CACHE: OnceLock<Mutex<StoreCacheMap>> = OnceLock::new();

pub fn open_or_cached_store(
    cacheroot: &Path,
    is_imageproxy: bool,
) -> Result<Arc<lms_cache::DbArtworkStore>, lms_cache::CacheError> {
    let canonical_root = validate_cacheroot(cacheroot)?;
    let cache = STORE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (canonical_root.clone(), is_imageproxy);

    {
        let guard = cache.lock().map_err(|_| {
            lms_cache::CacheError::Io(std::io::Error::other("store cache mutex poisoned"))
        })?;
        if let Some(store) = guard.get(&key) {
            return Ok(Arc::clone(store));
        }
    }

    let store = Arc::new(lms_cache::DbArtworkStore::open(
        &canonical_root,
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

fn read_frame_with_deadline(
    stream: &mut UnixStream,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT))?;
    let deadline = Instant::now() + SOCKET_IO_TIMEOUT;
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
                if let Some(pos) = slice.iter().position(|&b| b == b'\n') {
                    let needed = pos + 1;
                    if raw_line.len().saturating_add(needed) > MAX_FRAME_BYTES {
                        return Err("frame exceeded 32 MiB limit".into());
                    }
                    let Some(head) = slice.get(..needed) else {
                        return Err("invalid frame slice".into());
                    };
                    raw_line.extend_from_slice(head);
                    return Ok(raw_line);
                }
                if raw_line.len().saturating_add(n) >= MAX_FRAME_BYTES {
                    return Err("frame exceeded 32 MiB limit without newline".into());
                }
                raw_line.extend_from_slice(slice);
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

fn read_regular_input_bytes(path: &Path) -> Result<Vec<u8>, InputReadError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        // Linux asm-generic/fcntl.h O_NONBLOCK = 00004000 avoids FIFO hang.
        const O_NONBLOCK_FLAG: i32 = 0o4000;
        options.custom_flags(O_NONBLOCK_FLAG);
    }
    let mut file = options.open(path).map_err(InputReadError::Open)?;
    let meta = file.metadata().map_err(InputReadError::Stat)?;
    if !meta.is_file() {
        return Err(InputReadError::NotRegularFile);
    }
    let file_len = meta.len();
    if file_len > MAX_INPUT_FILE_BYTES {
        return Err(InputReadError::TooLarge);
    }

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
    match lms_image::extract_embedded_artwork_from_reader(&mut file, file_len) {
        Ok(Some(art)) if !art.data.is_empty() => Ok(art.data),
        Ok(Some(_)) => Err(InputReadError::EmptyArtwork),
        Ok(None) => Err(InputReadError::NoArtwork),
        Err(e) => Err(InputReadError::Extract(e)),
    }
}

fn handle_client(
    stream: &mut UnixStream,
    faster: bool,
    debug: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let raw_line = read_frame_with_deadline(stream)?;
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
        Some(_),
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

    let file =
        std::str::from_utf8(file_bytes).map_err(|e| format!("invalid UTF-8 in file path: {e}"))?;
    let mut spec =
        std::str::from_utf8(spec_bytes).map_err(|e| format!("invalid UTF-8 in spec: {e}"))?;
    let cacheroot = std::str::from_utf8(cacheroot_bytes)
        .map_err(|e| format!("invalid UTF-8 in cacheroot: {e}"))?;
    let cachekey = std::str::from_utf8(cachekey_bytes)
        .map_err(|e| format!("invalid UTF-8 in cachekey: {e}"))?;
    let data_str =
        std::str::from_utf8(data_bytes).map_err(|e| format!("invalid UTF-8 in data: {e}"))?;

    if spec.is_empty() {
        spec = "XxX";
    }

    if file.is_empty() || cacheroot.is_empty() || cachekey.is_empty() {
        return Err("missing required parameters".into());
    }

    let canonical_root = validate_cacheroot(Path::new(cacheroot))
        .map_err(|e| format!("invalid cacheroot '{cacheroot}': {e}"))?;

    let trimmed_data = data_str.trim();
    let (input_bytes, inline_len) = if !trimmed_data.is_empty() {
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(trimmed_data.as_bytes())
            .map_err(|e| format!("invalid base64 data: {e}"))?;
        if decoded.is_empty() {
            return Err("decoded inline image data is empty".into());
        }
        let len = decoded.len();
        (decoded, len)
    } else if file == "data" {
        return Err("missing inline image data".into());
    } else {
        let bytes = read_regular_input_bytes(Path::new(file)).map_err(|err| match err {
            InputReadError::Open(e) => format!("file open failed for {file}: {e}"),
            InputReadError::Stat(e) => format!("file stat failed for {file}: {e}"),
            InputReadError::NotRegularFile => {
                format!("input path is not a regular file: {file}")
            }
            InputReadError::TooLarge => {
                format!("input file exceeds 64 MiB limit: {file}")
            }
            InputReadError::Read(e) => format!("file read failed for {file}: {e}"),
            InputReadError::Seek(e) => format!("file seek failed for {file}: {e}"),
            InputReadError::Extract(e) => {
                format!("embedded artwork extraction failed for {file}: {e}")
            }
            InputReadError::NoArtwork => format!("no embedded artwork found in {file}"),
            InputReadError::EmptyArtwork => {
                format!("extracted embedded artwork is empty in {file}")
            }
        })?;
        (bytes, 0)
    };
    let input_source = lms_image::InputSource::Bytes(&input_bytes);

    let mut specs: Vec<String> = Vec::new();
    for s in spec.split(',') {
        if !s.is_empty() {
            specs.push(s.to_string());
        }
    }
    if specs.is_empty() {
        specs.push("XxX".to_string());
    }

    let is_imageproxy = cachekey.starts_with("imageproxy");
    if debug {
        eprintln!(
            "file={file}, spec={spec}, cacheroot={}, cachekey={cachekey}, \
             imageproxy={is_imageproxy}, imagedata={inline_len} bytes",
            canonical_root.display(),
        );
    }

    let store = open_or_cached_store(&canonical_root, is_imageproxy)?;
    lms_image::gdresize_to_cache(input_source, &specs, &store, cachekey, faster)?;

    Ok(())
}

#[inline]
pub fn process_client_request(
    stream: &mut UnixStream,
    faster: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    handle_client(stream, faster, false)
}

fn handle_client_connection(mut stream: UnixStream, faster: bool, debug: bool) {
    let started = debug.then(Instant::now);
    let res = handle_client(&mut stream, faster, debug);
    let response = match res {
        Ok(()) => {
            if let Some(t0) = started {
                eprintln!("OK ({:.3}s)", t0.elapsed().as_secs_f64());
            }
            "OK\r\n".to_string()
        }
        Err(e) => {
            if debug {
                eprintln!("Error: {e}");
            }
            format!("Error: {e}\r\n")
        }
    };
    let _ = stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT));
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

pub fn run_stdio(spec: &str, faster: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = Vec::new();
    std::io::stdin()
        .take(MAX_INPUT_FILE_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_INPUT_FILE_BYTES {
        return Err("stdin input exceeded 64MB limit".into());
    }
    let (fmt, bytes) = resize_one_shot(&buf, spec, faster)?;
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{fmt}")?;
    stdout.write_all(&bytes)?;
    stdout.flush()?;
    Ok(())
}

struct WorkerPool {
    sender: Option<std::sync::mpsc::SyncSender<UnixStream>>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl WorkerPool {
    fn spawn(faster: bool, debug: bool) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<UnixStream>(WORKER_QUEUE_CAPACITY);
        let rx = Arc::new(Mutex::new(receiver));
        let mut handles = Vec::with_capacity(WORKER_COUNT);
        for _ in 0..WORKER_COUNT {
            let rx = Arc::clone(&rx);
            let handle = std::thread::spawn(move || loop {
                let stream = {
                    let Ok(guard) = rx.lock() else {
                        break;
                    };
                    match guard.recv() {
                        Ok(s) => s,
                        Err(_) => break,
                    }
                };
                handle_client_connection(stream, faster, debug);
            });
            handles.push(handle);
        }
        Self {
            sender: Some(sender),
            handles,
        }
    }

    fn shutdown(mut self) {
        self.sender.take();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.sender.take();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

fn should_exit_server(initial_ppid: u32) -> bool {
    if SHUTDOWN_REQUESTED.load(Ordering::Acquire) {
        return true;
    }
    if initial_ppid != 1 && std::os::unix::process::parent_id() != initial_ppid {
        return true;
    }
    false
}

fn run_server_internal(
    sock_path: &Path,
    faster: bool,
    max_requests: Option<usize>,
    debug: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if max_requests.is_none() {
        install_signal_handlers();
    }
    prepare_socket_path(sock_path)?;

    let listener = {
        #[cfg(unix)]
        let _umask_guard = UmaskGuard::restrict_to_owner_rw()?;
        UnixListener::bind(sock_path)?
    };
    let _socket_guard = SocketGuard::new(sock_path.to_path_buf());
    std::fs::set_permissions(sock_path, PermissionsExt::from_mode(0o600))?;
    listener.set_nonblocking(true)?;

    if debug {
        eprintln!("listening on {}", sock_path.display());
    }

    let mut handled = 0;
    let initial_ppid = std::os::unix::process::parent_id();
    let pool = if max_requests.is_none() {
        Some(WorkerPool::spawn(faster, debug))
    } else {
        None
    };

    while match max_requests {
        Some(max) => handled < max,
        None => true,
    } {
        if should_exit_server(initial_ppid) {
            break;
        }
        let (stream, _) = match listener.accept() {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if should_exit_server(initial_ppid) {
            break;
        }
        if let Err(e) = stream.set_nonblocking(false) {
            if debug {
                eprintln!("failed to set blocking stream mode: {e}");
            }
            continue;
        }
        let _ = stream.set_read_timeout(Some(SOCKET_IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT));
        handled += 1;
        if let Some(ref worker_pool) = pool {
            if let Some(ref sender) = worker_pool.sender {
                match sender.try_send(stream) {
                    Ok(()) => {}
                    Err(std::sync::mpsc::TrySendError::Full(mut busy_stream)) => {
                        let _ = busy_stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT));
                        let _ = busy_stream.write_all(b"Error: server busy\r\n");
                        let _ = busy_stream.flush();
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                }
            } else {
                break;
            }
        } else {
            handle_client_connection(stream, faster, debug);
        }
    }

    if let Some(worker_pool) = pool {
        worker_pool.shutdown();
    }

    Ok(())
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
    read_regular_input_bytes(path).map_err(|err| match err {
        InputReadError::Open(e) => format!("Error opening file {}: {e}", path.display()),
        InputReadError::Stat(e) => format!("Error stating file {}: {e}", path.display()),
        InputReadError::NotRegularFile => {
            format!("Error: {} is not a regular file", path.display())
        }
        InputReadError::TooLarge => {
            format!("Error: {} exceeds 64 MiB limit", path.display())
        }
        InputReadError::Read(e) => format!("Error reading file {}: {e}", path.display()),
        InputReadError::Seek(e) => format!("Error seeking file {}: {e}", path.display()),
        InputReadError::Extract(e) => {
            format!(
                "Error extracting embedded artwork from {}: {e}",
                path.display()
            )
        }
        InputReadError::NoArtwork => format!(
            "Error extracting embedded artwork from {}: no embedded artwork found",
            path.display()
        ),
        InputReadError::EmptyArtwork => format!(
            "Error: extracted embedded artwork is empty in {}",
            path.display()
        ),
    })
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
    use super::parse_cli_args;
    use super::read_cli_input_image;
    use super::resize_one_shot;
    use super::run_server_n_requests;
    use image::DynamicImage;
    use image::ImageFormat as ImgFmt;
    use image::RgbImage;
    use std::io::Cursor;
    use std::io::Read;
    use std::io::Write;
    use std::net::Shutdown;
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Mutex;
    use std::sync::MutexGuard;
    use std::time::Duration;

    static TEST_FS_LOCK: Mutex<()> = Mutex::new(());

    fn test_fs_lock() -> MutexGuard<'static, ()> {
        TEST_FS_LOCK.lock().expect("test lock poisoned")
    }

    fn make_test_jpeg_bytes(w: u32, h: u32) -> Vec<u8> {
        let img =
            DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |_, _| image::Rgb([180, 90, 45])));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImgFmt::Jpeg)
            .expect("encode test JPEG");
        buf
    }

    fn spawn_server_and_wait(
        sock_path: &Path,
        faster: bool,
        requests: usize,
    ) -> std::thread::JoinHandle<()> {
        let server_sock = sock_path.to_path_buf();
        let handle = std::thread::spawn(move || {
            run_server_n_requests(&server_sock, faster, requests).expect("server loop failed");
        });
        let mut ready = false;
        for _ in 0..100 {
            if sock_path.exists() {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ready, "server socket did not appear");
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
        let _serial = test_fs_lock();
        let dir = tempfile::tempdir().expect("create tempdir");
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
        let _serial = test_fs_lock();
        let dir = tempfile::tempdir().expect("create tempdir");
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
        let _serial = test_fs_lock();
        let dir = tempfile::tempdir().expect("create tempdir");
        let sock_path = dir.path().join("sbs_artwork_perm_test");
        let cacheroot = dir.path().to_str().expect("UTF-8 tempdir path");
        let handle = spawn_server_and_wait(&sock_path, false, 1);

        // Wait briefly for chmod 0o600 immediately after UnixListener::bind.
        let mut mode = 0_u32;
        let mut is_sock = false;
        for _ in 0..50 {
            if let Ok(meta) = std::fs::symlink_metadata(&sock_path) {
                is_sock = meta.file_type().is_socket();
                mode = meta.permissions().mode() & 0o777;
                if mode == 0o600 {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }

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
        let _serial = test_fs_lock();
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
        let _serial = test_fs_lock();
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
        let _serial = test_fs_lock();
        let dir = tempfile::tempdir().expect("create tempdir");
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

        let cli_args = vec![
            "gdresized",
            "--cli-resize",
            "--spec",
            "100x100_o",
            "--file",
            "/run/cover.jpg",
            "--stdin",
            "--help",
        ];
        let cli_opts = parse_cli_args(cli_args).expect("parse valid cli-resize args");
        assert!(cli_opts.cli_resize);
        assert_eq!(cli_opts.spec.as_deref(), Some("100x100_o"));
        assert_eq!(cli_opts.file.as_deref(), Some(Path::new("/run/cover.jpg")));
        assert!(cli_opts.stdin);
        assert!(cli_opts.help);

        let invalid_cases = [
            vec!["gdresized", "--socket"],
            vec!["gdresized", "--socket", ""],
            vec!["gdresized", "--max-requests"],
            vec!["gdresized", "--max-requests", "not_a_number"],
            vec!["gdresized", "--spec"],
            vec!["gdresized", "--file"],
            vec!["gdresized", "--file", ""],
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
        let _serial = test_fs_lock();
        let dir = tempfile::tempdir().expect("create tempdir");
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

        // Verify SQLite store contains the resized image.
        let store = lms_cache::DbArtworkStore::open(dir.path(), false).expect("open artwork store");
        let record = store
            .get("music/99/cover")
            .expect("query artwork store")
            .expect("record must exist");
        assert_eq!(record.content_type, "jpg");
        assert!(!record.data.is_empty());
    }

    #[test]
    fn test_current_euid_and_umask_guard_restores_prior_mask() {
        let _serial = test_fs_lock();
        let proc_meta = std::fs::metadata("/proc/self").expect("stat /proc/self");
        assert_eq!(
            super::current_euid().expect("geteuid"),
            std::os::unix::fs::MetadataExt::uid(&proc_meta)
        );

        let before = {
            let _guard = super::UmaskGuard::restrict_to_owner_rw().expect("acquire umask guard");
            // JUSTIFICATION: Verify process umask inside UmaskGuard equals 0o177.
            // SAFETY:
            // 1. Preconditions: _guard holds UMASK_LOCK exclusively on thread.
            // 2. Bounds/Alignment: Scalar u32 bitmask argument only.
            // 3. Aliasing/Lifetime: No pointers or references cross FFI boundary.
            // 4. Thread safety: Protected by held UMASK_LOCK inside _guard.
            // 5. Return value: Returns active mask to verify 0o177 restriction.
            // 6. Failure mode: Infallible POSIX umask(2) restores on Drop.
            let active = unsafe { super::unix_ffi::umask(0o177) };
            assert_eq!(active, 0o177);
            active
        };
        assert_eq!(before, 0o177);

        let restored = {
            let guard = super::UmaskGuard::restrict_to_owner_rw().expect("re-acquire umask guard");
            guard.prev_mask
        };
        assert_ne!(
            restored,
            0o177 & 0o777,
            "expected non-0o177 test umask or restored mask"
        );
    }

    #[test]
    fn test_single_open_rejects_oversized_and_extracts_id3_apic() {
        let _serial = test_fs_lock();
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
        let mut apic_payload = Vec::new();
        apic_payload.push(0); // UTF-8/ISO-8859-1 encoding
        apic_payload.extend_from_slice(b"image/jpeg\0");
        apic_payload.push(3); // Front Cover
        apic_payload.push(0); // empty description
        apic_payload.extend_from_slice(&jpg);

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

        let mp3_path = dir.path().join("track.mp3");
        std::fs::write(&mp3_path, &mp3_bytes).expect("write test mp3");
        let extracted = read_cli_input_image(&mp3_path).expect("extract ID3 APIC");
        assert_eq!(extracted, jpg);
    }
}
