use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Mutex, MutexGuard, OnceLock},
};

use anyhow::{Context, Result, anyhow};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    time::{Duration, timeout},
};
use tracing_subscriber::fmt::writer::EitherWriter;
use xxhash_rust::xxh3::xxh3_64;

const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
static LOG: OnceLock<Mutex<RotatingLog>> = OnceLock::new();

pub(super) fn init() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "browser_mcp=info".into()),
        )
        .with_ansi(false)
        // A managed broker outlives the adapter that launched it. Once durable
        // diagnostics are enabled, never also write to that adapter's stderr:
        // tracing reports a failed write with eprintln!, which can itself panic.
        .with_writer(|| match LOG.get() {
            Some(log) => EitherWriter::A(BrokerWriter(log)),
            None => EitherWriter::B(std::io::stderr()),
        })
        .init();
}

// Hold the mutex only during I/O, not while the caller formats an event.
struct BrokerWriter<'a>(&'a Mutex<RotatingLog>);

impl BrokerWriter<'_> {
    fn lock(&self) -> MutexGuard<'_, RotatingLog> {
        self.0.lock().unwrap_or_else(|poisoned| {
            let mut log = poisoned.into_inner();
            let _ = writeln!(
                log,
                "WARN browser broker diagnostics recovered a poisoned log lock"
            );
            self.0.clear_poison();
            log
        })
    }
}

impl Write for BrokerWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let decoded = String::from_utf8_lossy(bytes);
        let message = super::privacy::known(&decoded);
        self.lock().write_all(message.as_bytes())?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.lock().flush()
    }
    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> io::Result<()> {
        let formatted = std::fmt::format(args);
        let message = super::privacy::known(&formatted);
        self.lock().write_all(message.as_bytes())
    }
}

pub(super) fn location(profile: &Path, prefix: &str) -> Result<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| anyhow!("HOME or XDG_STATE_HOME is required for broker diagnostics"))?;
    let directory = state.join("browser-mcp");
    let id = xxh3_64(profile.as_os_str().as_encoded_bytes());
    Ok(directory.join(format!("{prefix}-{id:016x}.log")))
}

fn log_path(profile: &Path, prefix: &str) -> Result<PathBuf> {
    let path = location(profile, prefix)?;
    let directory = path.parent().expect("log directory");
    fs::create_dir_all(directory).context("failed to create browser broker log directory")?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

pub(super) fn enable(profile: &Path) -> Result<()> {
    let path = log_path(profile, "broker")?;
    let log = RotatingLog::open(path.clone(), MAX_LOG_BYTES)
        .with_context(|| format!("failed to open browser broker log {}", path.display()))?;
    LOG.set(Mutex::new(log))
        .map_err(|_| anyhow!("browser broker diagnostics already initialized"))?;
    tracing::info!(pid = std::process::id(), version = env!("CARGO_PKG_VERSION"), log = %path.display(), "persistent browser broker diagnostics enabled");
    Ok(())
}

pub(super) async fn capture_worker_stderr(
    mut stderr: tokio::process::ChildStderr,
) -> io::Result<()> {
    let mut buffer = [0; 8192];
    loop {
        let count = stderr.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        tracing::info!(target: "browser_mcp::worker", diagnostic = %String::from_utf8_lossy(&buffer[..count]).trim_end(), "Chrome DevTools MCP stderr");
    }
}

/// The browser owns the pipe writer; a separate process owns the reader so
/// replacing the broker cannot close it. EOF ends the writer automatically.
pub(super) async fn browser_stderr(profile: &Path) -> Result<Stdio> {
    let path = log_path(profile, "browser")?;
    let mut command = Command::new("/proc/self/exe");
    command
        .arg("browser-stderr")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(false);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut setup = StderrSetup(Some(
        command
            .spawn()
            .context("failed to start browser stderr writer")?,
    ));
    let writer = setup.0.as_mut().expect("setup owns the stderr writer");
    let mut ready = [0; 6];
    timeout(
        Duration::from_secs(5),
        writer
            .stdout
            .as_mut()
            .expect("writer stdout was piped")
            .read_exact(&mut ready),
    )
    .await
    .context("browser stderr writer did not become ready")?
    .context("browser stderr writer exited before opening its log")?;
    anyhow::ensure!(
        &ready == b"ready\n",
        "invalid browser stderr writer readiness response"
    );
    let pipe = writer
        .stdin
        .take()
        .expect("writer stdin was piped")
        .try_into()
        .context("failed to transfer browser stderr pipe")?;
    tracing::info!(pid = writer.id(), log = %path.display(), "browser stderr writer ready");
    drop(setup.0.take());
    Ok(pipe)
}

struct StderrSetup(Option<tokio::process::Child>);

impl Drop for StderrSetup {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut()
            && let Err(error) = child.start_kill()
        {
            tracing::warn!(%error, "failed to stop uninitialized browser stderr writer");
        }
    }
}

pub(super) fn run_browser_stderr(path: &Path) -> Result<()> {
    // A crashed browser's descendants can retain stderr briefly. Serialize
    // generations so two writers cannot race rotation of the same log.
    let lease = RotatingLog::open_file(&path.with_extension("log.lock"))?;
    if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX) } == -1 {
        return Err(io::Error::last_os_error()).context("failed to lock browser stderr log");
    }
    let mut log = RotatingLog::open(path.to_owned(), MAX_LOG_BYTES)
        .with_context(|| format!("failed to open browser stderr log {}", path.display()))?;
    let mut ready = io::stdout().lock();
    ready.write_all(b"ready\n")?;
    ready.flush()?;
    drop(ready);
    io::copy(&mut io::stdin().lock(), &mut log).context("failed to capture browser stderr")?;
    log.flush()?;
    Ok(())
}

struct RotatingLog {
    path: PathBuf,
    file: File,
    bytes: u64,
    limit: u64,
}

impl RotatingLog {
    fn open(path: PathBuf, limit: u64) -> io::Result<Self> {
        let file = Self::open_file(&path)?;
        let bytes = file.metadata()?.len();
        Ok(Self {
            path,
            file,
            bytes,
            limit,
        })
    }

    fn open_file(path: &Path) -> io::Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
}

impl Write for RotatingLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes >= self.limit {
            fs::rename(&self.path, self.path.with_extension("log.1"))?;
            self.file = Self::open_file(&self.path)?;
            self.bytes = 0;
        }
        let written = self.file.write(bytes)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_writer_recovers_a_poisoned_log() {
        let directory =
            std::env::temp_dir().join(format!("browser-mcp-poisoned-log-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("broker.log");
        let log = Mutex::new(RotatingLog::open(path.clone(), MAX_LOG_BYTES).unwrap());
        let _ = std::panic::catch_unwind(|| {
            let _guard = log.lock().unwrap();
            panic!("simulated diagnostic panic");
        });
        assert!(log.is_poisoned());
        BrokerWriter(&log)
            .write_all(b"request completed\n")
            .unwrap();
        BrokerWriter(&log).flush().unwrap();
        assert!(!log.is_poisoned());
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "WARN browser broker diagnostics recovered a poisoned log lock\nrequest completed\n"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn formatting_panic_does_not_poison_diagnostics() {
        struct PanickingDisplay;
        impl std::fmt::Display for PanickingDisplay {
            fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                panic!("simulated formatting panic");
            }
        }

        let directory =
            std::env::temp_dir().join(format!("browser-mcp-format-panic-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("broker.log");
        let log = Mutex::new(RotatingLog::open(path.clone(), MAX_LOG_BYTES).unwrap());
        let result = std::panic::catch_unwind(|| {
            let mut writer = BrokerWriter(&log);
            write!(writer, "{PanickingDisplay}").unwrap();
        });
        assert!(result.is_err());
        assert!(!log.is_poisoned());
        BrokerWriter(&log)
            .write_all(b"request completed\n")
            .unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "request completed\n");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persistent_diagnostics_survive_closed_host_stderr() {
        const CHILD: &str = "BROWSER_MCP_CLOSED_STDERR_TEST";
        if std::env::var_os(CHILD).is_some() {
            init();
            enable(Path::new("test-profile")).unwrap();
            tracing::info!("request completed after host disconnected");
            return;
        }
        let directory =
            std::env::temp_dir().join(format!("browser-mcp-closed-stderr-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(reader);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "logging::tests::persistent_diagnostics_survive_closed_host_stderr",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("XDG_STATE_HOME", &directory)
            .stdout(Stdio::null())
            .stderr(std::os::fd::OwnedFd::from(writer))
            .status()
            .unwrap();
        assert!(
            status.success(),
            "diagnostics panicked on disconnected host stderr"
        );
        let path = fs::read_dir(directory.join("browser-mcp"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("request completed after host disconnected")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn logs_rotate_and_remain_private() {
        let directory =
            std::env::temp_dir().join(format!("browser-mcp-log-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("broker.log");
        let mut log = RotatingLog::open(path.clone(), 4).unwrap();
        log.write_all(b"one\n").unwrap();
        log.write_all(b"two\n").unwrap();
        log.write_all(b"three\n").unwrap();
        drop(log);
        assert_eq!(fs::read(&path).unwrap(), b"three\n");
        assert_eq!(fs::read(path.with_extension("log.1")).unwrap(), b"two\n");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
