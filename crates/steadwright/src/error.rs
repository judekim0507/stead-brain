use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{message}")]
    Timeout { message: String },
    #[error(transparent)]
    Protocol(#[from] steadwright_cdp::CdpError),
    #[error("{0}")]
    Evaluation(String),
    #[error("Target page, context or browser has been closed")]
    TargetClosed,
    #[error("Frame was detached")]
    FrameDetached,
    #[error("{0}")]
    Navigation(String),
    #[error("{0}")]
    InvalidArgument(String),
    #[error("Not implemented: {0}")]
    NotImplemented(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct Deadline {
    end: Instant,
    timeout: Duration,
}

impl Deadline {
    pub fn new(timeout: Duration) -> Self {
        Self {
            end: Instant::now() + timeout,
            timeout,
        }
    }

    pub fn timeout(self) -> Duration {
        self.timeout
    }
    pub fn remaining(self) -> Duration {
        self.end.saturating_duration_since(Instant::now())
    }
    pub fn expired(self) -> bool {
        Instant::now() >= self.end
    }

    pub(crate) async fn run<T>(
        self,
        api: &str,
        future: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        tokio::time::timeout(self.remaining(), future)
            .await
            .map_err(|_| Error::timeout(api, self.timeout))?
    }
}

impl Error {
    pub(crate) fn timeout(api: &str, timeout: Duration) -> Self {
        Self::Timeout {
            message: format!("{api}: Timeout {}ms exceeded.", timeout.as_millis()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_message_names_the_api_and_milliseconds() {
        assert_eq!(
            Error::timeout("page.goto", Duration::from_secs(30)).to_string(),
            "page.goto: Timeout 30000ms exceeded."
        );
    }
}
