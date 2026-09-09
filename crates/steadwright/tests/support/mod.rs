#![allow(dead_code)]

use std::collections::HashMap;
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use steadwright::Browser;
use steadwright_cdp::chromium::{self, LaunchError, LaunchOptions, LaunchedChromium};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

static FIXTURE: OnceLock<Option<&'static Fixture>> = OnceLock::new();
static SKIP_NOTICE: OnceLock<()> = OnceLock::new();

/// A shared browser and two HTTP origins for the live integration tests.
///
/// Tests deliberately create their own pages. A fixture lease serializes them
/// because they share the default context, while the process and listeners
/// remain alive on a dedicated runtime until the test binary exits.
pub struct Fixture {
    pub browser: Browser,
    pub origin_a: String,
    pub origin_b: String,
    test_lock: Arc<tokio::sync::Mutex<()>>,
    _chromium: LaunchedChromium,
    _servers: [JoinHandle<()>; 2],
}

pub struct FixtureLease {
    fixture: &'static Fixture,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl Deref for FixtureLease {
    type Target = Fixture;

    fn deref(&self) -> &Self::Target {
        self.fixture
    }
}

impl Fixture {
    /// Returns the process-wide fixture, or `None` when Chromium is unavailable.
    /// A missing browser is an intentional, visible skip rather than a failure.
    pub async fn get() -> Option<FixtureLease> {
        let fixture = tokio::task::spawn_blocking(|| *FIXTURE.get_or_init(start_fixture_thread))
            .await
            .expect("steadwright fixture initialization task panicked")?;
        let guard = fixture.test_lock.clone().lock_owned().await;
        Some(FixtureLease {
            fixture,
            _guard: guard,
        })
    }

    async fn start() -> Result<Self, FixtureError> {
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures");
        let listener_a = TcpListener::bind(("127.0.0.1", 0)).await?;
        let listener_b = TcpListener::bind(("127.0.0.1", 0)).await?;
        let origin_a = format!("http://{}", listener_a.local_addr()?);
        let origin_b = format!("http://{}", listener_b.local_addr()?);

        let server_a = spawn_server(listener_a, fixtures.clone(), origin_b.clone());
        let server_b = spawn_server(listener_b, fixtures, origin_b.clone());
        let chromium = match chromium::launch(LaunchOptions::default()).await {
            Ok(chromium) => chromium,
            Err(LaunchError::EarlyExit(_) | LaunchError::DevToolsTimeout)
                if std::env::var_os("STEADWRIGHT_CHROMIUM").is_none() =>
            {
                let chrome =
                    PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
                if !chrome.is_file() {
                    return Err(FixtureError::Chromium(LaunchError::ExecutableNotFound));
                }
                chromium::launch(LaunchOptions {
                    executable: Some(chrome),
                    ..LaunchOptions::default()
                })
                .await?
            }
            Err(error) => return Err(error.into()),
        };
        let browser = Browser::connect_ws(&chromium.ws_url).await?;

        Ok(Self {
            browser,
            origin_a,
            origin_b,
            test_lock: Arc::new(tokio::sync::Mutex::new(())),
            _chromium: chromium,
            _servers: [server_a, server_b],
        })
    }

    pub fn url_a(&self, path: &str) -> String {
        format!("{}{}", self.origin_a, normalized_path(path))
    }

    pub fn url_b(&self, path: &str) -> String {
        format!("{}{}", self.origin_b, normalized_path(path))
    }

    pub async fn new_page(&self) -> steadwright::Result<steadwright::Page> {
        self.browser.default_context().new_page().await
    }
}

fn start_fixture_thread() -> Option<&'static Fixture> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("steadwright-live-fixture".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build fixture Tokio runtime");
            match runtime.block_on(Fixture::start()) {
                Ok(fixture) => {
                    let fixture = Box::leak(Box::new(fixture));
                    sender
                        .send(Ok(Some(&*fixture)))
                        .expect("fixture initializer receiver dropped");
                    runtime.block_on(std::future::pending::<()>());
                }
                Err(FixtureError::Chromium(error)) => {
                    eprintln!("steadwright live tests skipped: Chromium unavailable: {error}");
                    sender
                        .send(Ok(None))
                        .expect("fixture initializer receiver dropped");
                }
                Err(error) => {
                    sender
                        .send(Err(error.to_string()))
                        .expect("fixture initializer receiver dropped");
                }
            }
        })
        .expect("failed to spawn fixture runtime thread");

    match receiver
        .recv()
        .expect("fixture runtime exited before initialization")
    {
        Ok(Some(fixture)) => Some(fixture),
        Ok(None) => {
            SKIP_NOTICE.get_or_init(|| {
                eprintln!("steadwright live tests skipped; set STEADWRIGHT_CHROMIUM to a launchable binary");
            });
            None
        }
        Err(error) => panic!("failed to start steadwright fixture: {error}"),
    }
}

#[derive(Debug, thiserror::Error)]
enum FixtureError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Chromium(#[from] LaunchError),
    #[error(transparent)]
    Steadwright(#[from] steadwright::Error),
}

fn normalized_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

fn spawn_server(listener: TcpListener, fixtures: PathBuf, origin_b: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let fixtures = fixtures.clone();
            let origin_b = origin_b.clone();
            tokio::spawn(async move {
                if let Err(error) = serve_connection(stream, &fixtures, &origin_b).await {
                    eprintln!("steadwright fixture server error: {error}");
                }
            });
        }
    })
}

async fn serve_connection(
    mut stream: TcpStream,
    fixtures: &Path,
    origin_b: &str,
) -> io::Result<()> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&chunk[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") || request.len() >= 16 * 1024 {
            break;
        }
    }

    let request = String::from_utf8_lossy(&request);
    let Some(target) = request
        .lines()
        .next()
        .and_then(|line| line.split_ascii_whitespace().nth(1))
    else {
        return write_response(
            &mut stream,
            400,
            "text/plain; charset=utf-8",
            b"bad request",
            &[],
        )
        .await;
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = parse_query(query);

    if path == "/slow" {
        let delay = query
            .get("ms")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            .min(30_000);
        tokio::time::sleep(Duration::from_millis(delay)).await;
        let body = format!(
            "<!doctype html><html><head><title>Slow {delay}</title></head><body>slow response after {delay}ms</body></html>"
        );
        return write_response(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            body.as_bytes(),
            &[],
        )
        .await;
    }

    if path == "/redirect" {
        let destination = query.get("to").map(String::as_str).unwrap_or("/index.html");
        return write_response(
            &mut stream,
            302,
            "text/plain; charset=utf-8",
            b"redirecting",
            &[("Location", destination)],
        )
        .await;
    }

    if path == "/favicon.ico" {
        return write_response(&mut stream, 204, "image/x-icon", b"", &[]).await;
    }

    if path == "/download" {
        return write_response(
            &mut stream,
            200,
            "text/plain; charset=utf-8",
            b"steadwright download",
            &[(
                "Content-Disposition",
                "attachment; filename=\"steadwright.txt\"",
            )],
        )
        .await;
    }

    let relative = path.trim_start_matches('/');
    if relative.is_empty() || relative.contains("..") || relative.contains('\\') {
        return write_response(
            &mut stream,
            404,
            "text/plain; charset=utf-8",
            b"not found",
            &[],
        )
        .await;
    }
    let fixture = fixtures.join(relative);
    match tokio::fs::read(&fixture).await {
        Ok(mut body) => {
            if relative == "iframe-parent.html" {
                body = String::from_utf8_lossy(&body)
                    .replace("{origin_b}", origin_b)
                    .into_bytes();
            }
            write_response(&mut stream, 200, content_type(relative), &body, &[]).await
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            write_response(
                &mut stream,
                404,
                "text/plain; charset=utf-8",
                b"not found",
                &[],
            )
            .await
        }
        Err(error) => Err(error),
    }
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Unknown",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\nCache-Control: no-store\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

fn content_type(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|value| value.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => output.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let decoded = hex(bytes[index + 1]).zip(hex(bytes[index + 2]));
                if let Some((high, low)) = decoded {
                    output.push((high << 4) | low);
                    index += 2;
                } else {
                    output.push(bytes[index]);
                }
            }
            byte => output.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{content_type, parse_query, percent_decode};

    #[test]
    fn decodes_query_strings_without_an_external_url_dependency() {
        assert_eq!(percent_decode("%2Fspa.html%3Fx%3D1+2"), "/spa.html?x=1 2");
        let query = parse_query("to=%2Findex.html&name=stead+wright");
        assert_eq!(query["to"], "/index.html");
        assert_eq!(query["name"], "stead wright");
    }

    #[test]
    fn serves_expected_content_types() {
        assert_eq!(content_type("fixture.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("image.png"), "image/png");
    }
}
