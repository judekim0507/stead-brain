use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

const STEAD_CHROMIUM: &str = "/Users/judekim/Developer/Stead/stead-macos/build/src/out/Default/Stead.app/Contents/MacOS/Stead";
const GOOGLE_CHROME: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const DEVTOOLS_PREFIX: &str = "DevTools listening on ";

#[derive(Clone, Debug)]
pub struct LaunchOptions {
    pub executable: Option<PathBuf>,
    pub headless: bool,
    pub user_data_dir: Option<PathBuf>,
    pub extra_args: Vec<String>,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            executable: None,
            headless: true,
            user_data_dir: None,
            extra_args: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("no Chromium executable found")]
    ExecutableNotFound,
    #[error("failed to create temporary Chromium profile: {0}")]
    TemporaryProfile(#[source] std::io::Error),
    #[error("failed to launch Chromium: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("Chromium stderr was not piped")]
    MissingStderr,
    #[error("timed out waiting for Chromium's DevTools endpoint")]
    DevToolsTimeout,
    #[error("Chromium exited before publishing its DevTools endpoint{0}")]
    EarlyExit(String),
    #[error("failed reading Chromium stderr: {0}")]
    Stderr(#[source] std::io::Error),
}

pub struct LaunchedChromium {
    pub ws_url: String,
    pub child: Child,
    temporary_profile: Option<tempfile::TempDir>,
}

impl Drop for LaunchedChromium {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        if let Some(profile) = self.temporary_profile.take() {
            let _ = profile.close();
        }
    }
}

pub fn find_executable() -> Option<PathBuf> {
    std::env::var_os("STEADWRIGHT_CHROMIUM")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            [STEAD_CHROMIUM, GOOGLE_CHROME]
                .into_iter()
                .map(PathBuf::from)
                .find(|path| path.is_file())
        })
}

pub async fn launch(options: LaunchOptions) -> Result<LaunchedChromium, LaunchError> {
    let executable = options
        .executable
        .or_else(find_executable)
        .ok_or(LaunchError::ExecutableNotFound)?;
    let (user_data_dir, temporary_profile) = match options.user_data_dir {
        Some(path) => (path, None),
        None => {
            let profile = tempfile::Builder::new()
                .prefix("steadwright-chromium-")
                .tempdir()
                .map_err(LaunchError::TemporaryProfile)?;
            (profile.path().to_owned(), Some(profile))
        }
    };

    let mut command = Command::new(executable);
    command
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check");
    if options.headless {
        command.arg("--headless=new");
    }
    command.args(options.extra_args).arg("about:blank");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().map_err(LaunchError::Spawn)?;
    let stderr = child.stderr.take().ok_or(LaunchError::MissingStderr)?;
    let mut lines = BufReader::new(stderr).lines();
    let endpoint = tokio::time::timeout(Duration::from_secs(10), async {
        let mut diagnostics = Vec::new();
        loop {
            match lines.next_line().await.map_err(LaunchError::Stderr)? {
                Some(line) => {
                    if let Some(url) = line.strip_prefix(DEVTOOLS_PREFIX) {
                        return Ok(url.to_owned());
                    }
                    if diagnostics.len() < 8 {
                        diagnostics.push(line);
                    }
                }
                None => {
                    let detail = if diagnostics.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", diagnostics.join(" | "))
                    };
                    return Err(LaunchError::EarlyExit(detail));
                }
            }
        }
    })
    .await;

    let ws_url = match endpoint {
        Ok(result) => result?,
        Err(_) => {
            let _ = child.start_kill();
            return Err(LaunchError::DevToolsTimeout);
        }
    };
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

    if let Some(pid) = child.id() {
        register_exit_kill(pid);
    }
    Ok(LaunchedChromium {
        ws_url,
        child,
        temporary_profile,
    })
}

/// Pids of launched browsers, killed when this process exits normally.
///
/// Test fixtures commonly keep a `LaunchedChromium` in a static, which is
/// never dropped, and `libtest` exits via `process::exit`, so `Drop` alone
/// leaks a headless browser per test binary. `atexit` runs on that path.
static EXIT_KILL_PIDS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

#[allow(unsafe_code)]
fn register_exit_kill(pid: u32) {
    let mut pids = EXIT_KILL_PIDS.lock().unwrap_or_else(|e| e.into_inner());
    if pids.is_empty() {
        // SAFETY: registering a plain `extern "C"` function with no arguments.
        unsafe {
            libc::atexit(kill_launched_at_exit);
        }
    }
    pids.push(pid);
}

#[allow(unsafe_code)]
extern "C" fn kill_launched_at_exit() {
    let pids = EXIT_KILL_PIDS.lock().unwrap_or_else(|e| e.into_inner());
    for pid in pids.iter() {
        // SAFETY: sending SIGKILL to a pid this process spawned; a stale pid
        // that already exited returns ESRCH and is ignored.
        unsafe {
            libc::kill(*pid as libc::pid_t, libc::SIGKILL);
        }
    }
}
