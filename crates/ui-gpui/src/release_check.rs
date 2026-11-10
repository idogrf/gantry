//! The check for a newer release: one blocking GET of GitHub's releases list, with the last `ETag`.

use std::time::Duration;

use app_core::release::{CheckStatus, Release, parse_releases};
use gpui::Global;

/// GitHub's list of published releases.
const ENDPOINT: &str = "https://api.github.com/repos/idogrf/gantry/releases?per_page=20";

/// How long the whole request may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Wait after the window opens before the first check.
pub(crate) const FIRST_DELAY: Duration = Duration::from_secs(30);

/// How often the timer asks `app_core::release::due` whether a check is due. Short, so a laptop that was asleep checks
/// soon after it wakes; the check itself runs once per `release::CHECK_EVERY`.
pub(crate) const TICK: Duration = Duration::from_secs(5 * 60);

/// What the Settings "Check now" row shows; the workspace keeps it current.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckRow {
    /// The status line.
    pub(crate) status: CheckStatus,
    /// The newest release's page, when one is newer.
    pub(crate) url: Option<String>,
}

impl Default for CheckRow {
    fn default() -> Self {
        CheckRow { status: CheckStatus::Idle, url: None }
    }
}

impl Global for CheckRow {}

/// What a request came back with.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fetched {
    /// A fresh list.
    Releases {
        /// The response body.
        body: String,
        /// Its `ETag`, to send next time.
        etag: Option<String>,
    },
    /// The list has not changed since the `ETag` sent.
    NotModified,
    /// Anything else: no network, a bad status, a timeout.
    Failed,
}

/// Takes in what a request came back with: a fresh list replaces both values, anything else keeps them.
pub(crate) fn apply_result(current: &mut Vec<Release>, etag: &mut Option<String>, fetched: Fetched) {
    if let Fetched::Releases { body, etag: new } = fetched {
        *current = parse_releases(&body);
        *etag = new;
    }
}

/// The address to ask: the constant, or in debug builds `GANTRY_UPDATE_URL` when set.
pub(crate) fn endpoint() -> String {
    #[cfg(debug_assertions)]
    return endpoint_from(std::env::var("GANTRY_UPDATE_URL").ok());
    #[cfg(not(debug_assertions))]
    return endpoint_from(None);
}

fn endpoint_from(override_url: Option<String>) -> String {
    override_url.filter(|url| !url.is_empty()).unwrap_or_else(|| ENDPOINT.to_owned())
}

/// Asks `url` for the release list, sending `etag` as `If-None-Match`. Blocks for up to ten seconds.
pub(crate) fn fetch(url: &str, etag: Option<&str>) -> Fetched {
    fetch_with(url, etag, TIMEOUT)
}

fn fetch_with(url: &str, etag: Option<&str>, timeout: Duration) -> Fetched {
    let agent: ureq::Agent =
        ureq::Agent::config_builder().timeout_global(Some(timeout)).http_status_as_error(false).build().into();
    let mut request = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", concat!("gantry/", env!("CARGO_PKG_VERSION")));
    if let Some(etag) = etag {
        request = request.header("If-None-Match", etag);
    }
    let mut response = match request.call() {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!("release check failed: {error}");
            return Fetched::Failed;
        }
    };
    match response.status().as_u16() {
        304 => Fetched::NotModified,
        200 => {
            let etag = response.headers().get("etag").and_then(|value| value.to_str().ok()).map(str::to_owned);
            match response.body_mut().read_to_string() {
                Ok(body) => Fetched::Releases { body, etag },
                Err(error) => {
                    tracing::debug!("release check body failed: {error}");
                    Fetched::Failed
                }
            }
        }
        status => {
            tracing::debug!("release check answered {status}");
            Fetched::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// Serves one canned response on a local port; the request text comes back on the channel.
    fn serve(response: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/releases", listener.local_addr().unwrap());
        let (sent, seen) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&chunk[..n]),
                }
            }
            let _ = sent.send(String::from_utf8_lossy(&request).into_owned());
            if response.is_empty() {
                std::thread::sleep(Duration::from_secs(3));
            } else {
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (url, seen)
    }

    #[test]
    fn fetch_200_returns_body_and_etag() {
        let (url, seen) = serve("HTTP/1.1 200 OK\r\nETag: \"abc\"\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]");
        let fetched = fetch(&url, None);
        assert_eq!(fetched, Fetched::Releases { body: "[]".into(), etag: Some("\"abc\"".into()) });
        let request = seen.recv().unwrap().to_lowercase();
        assert!(request.contains("user-agent: gantry/"), "{request}");
        assert!(request.contains("accept: application/vnd.github+json"), "{request}");
        assert!(!request.contains("if-none-match"), "{request}");
    }

    #[test]
    fn fetch_sends_if_none_match_and_maps_304() {
        let (url, seen) = serve("HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n");
        assert_eq!(fetch(&url, Some("\"abc\"")), Fetched::NotModified);
        assert!(seen.recv().unwrap().to_lowercase().contains("if-none-match: \"abc\""));
    }

    #[test]
    fn fetch_500_and_garbage_are_failed() {
        let (url, _) = serve("HTTP/1.1 500 Oops\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        assert_eq!(fetch(&url, None), Fetched::Failed);
        let (url, _) = serve("not http at all");
        assert_eq!(fetch(&url, None), Fetched::Failed);
    }

    #[test]
    fn fetch_refused_port_is_failed() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert_eq!(fetch(&format!("http://127.0.0.1:{port}/"), None), Fetched::Failed);
    }

    #[test]
    fn fetch_stalled_server_times_out() {
        let (url, _) = serve("");
        let started = std::time::Instant::now();
        assert_eq!(fetch_with(&url, None, Duration::from_millis(300)), Fetched::Failed);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    fn release_list(tag: &str) -> String {
        format!(r#"[{{"tag_name":"{tag}","draft":false,"prerelease":false}}]"#)
    }

    #[test]
    fn apply_replaces_on_new_body() {
        let (mut current, mut etag) = (parse_releases(&release_list("v0.6.0")), Some("\"old\"".to_owned()));
        let fetched = Fetched::Releases { body: release_list("v0.7.0"), etag: Some("\"new\"".into()) };
        apply_result(&mut current, &mut etag, fetched);
        assert_eq!(current[0].version.to_string(), "0.7.0");
        assert_eq!(etag.as_deref(), Some("\"new\""));
    }

    #[test]
    fn apply_keeps_on_not_modified() {
        let (mut current, mut etag) = (parse_releases(&release_list("v0.6.0")), Some("\"old\"".to_owned()));
        apply_result(&mut current, &mut etag, Fetched::NotModified);
        assert_eq!((current.len(), etag.as_deref()), (1, Some("\"old\"")));
    }

    #[test]
    fn apply_keeps_previous_on_failure() {
        let (mut current, mut etag) = (parse_releases(&release_list("v0.6.0")), Some("\"old\"".to_owned()));
        apply_result(&mut current, &mut etag, Fetched::Failed);
        assert_eq!((current.len(), etag.as_deref()), (1, Some("\"old\"")));
    }

    #[test]
    fn endpoint_uses_the_override_only_when_given() {
        assert_eq!(endpoint_from(None), ENDPOINT);
        assert_eq!(endpoint_from(Some(String::new())), ENDPOINT);
        assert_eq!(endpoint_from(Some("http://127.0.0.1:9/x".into())), "http://127.0.0.1:9/x");
    }
}
