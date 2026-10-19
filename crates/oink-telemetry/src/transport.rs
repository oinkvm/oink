//! The agent owns this transport. No automatic retry, redirects, SDK or cookies.
use crate::{
    store::{Delivery, Submission},
    *,
};
use reqwest::{Client, Url};
use std::time::Duration;

#[derive(Clone)]
pub struct Endpoint {
    url: Url,
    public_key: String,
    pub region: Region,
}
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}
impl Endpoint {
    pub fn parse(dsn: &str, region: Region, policy: u16) -> Result<Self> {
        if policy != POLICY_VERSION || dsn.len() > 512 {
            return Err(Error::Invalid);
        }
        let u = Url::parse(dsn).map_err(|_| Error::Invalid)?;
        let host = u.host_str().ok_or(Error::Invalid)?;
        let suffix = match region {
            Region::Eu => ".ingest.de.sentry.io",
            Region::Us => ".ingest.us.sentry.io",
        };
        let org = host.strip_suffix(suffix).ok_or(Error::Invalid)?;
        if u.scheme() != "https"
            || u.port().is_some()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
            || !org
                .strip_prefix('o')
                .is_some_and(|v| !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()))
            || u.username().len() != 32
            || !u.username().bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(Error::Invalid);
        }
        let project = u.path().strip_prefix('/').ok_or(Error::Invalid)?;
        if project.is_empty() || !project.bytes().all(|c| c.is_ascii_digit()) {
            return Err(Error::Invalid);
        }
        let url = Url::parse(&format!("https://{host}/api/{project}/envelope/")).map_err(|_| Error::Invalid)?;
        Ok(Self {
            url,
            public_key: u.username().into(),
            region,
        })
    }
    /// Build configuration only. No runtime environment DSN or remote config.
    pub fn compiled() -> Option<Self> {
        let dsn = option_env!("OINK_REPORT_DSN")?;
        let region = match option_env!("OINK_REPORT_REGION")? {
            "eu" => Region::Eu,
            "us" => Region::Us,
            _ => return None,
        };
        Self::parse(dsn, region, POLICY_VERSION).ok()
    }
}
#[derive(Debug)]
pub struct Https {
    client: Client,
    endpoint: Endpoint,
}
impl Https {
    pub fn new(endpoint: Endpoint) -> Result<Self> {
        let client = Client::builder()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .referer(false)
            .user_agent("Oink-Reports/3")
            .build()
            .map_err(|_| Error::Invalid)?;
        Ok(Self { client, endpoint })
    }
    pub async fn send(&self, submission: &Submission) -> Delivery {
        // Final independent schema revalidation, including persisted state. No
        // caller-supplied request bytes, headers, labels or item kinds are used.
        let Ok(bytes) = wire::encode(&submission.report) else {
            return Delivery::Drop;
        };
        if bytes != submission.bytes {
            return Delivery::Drop;
        }
        let response = self
            .client
            .post(self.endpoint.url.clone())
            .header("Content-Type", "application/x-sentry-envelope")
            .header(
                "X-Sentry-Auth",
                format!("Sentry sentry_version=7,sentry_key={}", self.endpoint.public_key),
            )
            .body(bytes)
            .send()
            .await;
        let Ok(response) = response else { return Delivery::Retry };
        let code = response.status().as_u16();
        let limited = limits(
            response
                .headers()
                .get("X-Sentry-Rate-Limits")
                .and_then(|s| s.to_str().ok()),
            response.headers().get("Retry-After").and_then(|s| s.to_str().ok()),
            code,
            now(),
        );
        // Discard the body without reading or logging it: zero response bytes is
        // below the 16 KiB cap, including malicious unbounded/chunked bodies.
        drop(response);
        if let Some(result) = limited {
            return result;
        }
        match code {
            200..=299 => Delivery::Accepted,
            400 | 401 | 403 => Delivery::InvalidProject,
            413 => Delivery::TooLarge,
            500..=599 => Delivery::Retry,
            _ => Delivery::Drop,
        }
    }
}
fn limits(header: Option<&str>, retry: Option<&str>, status: u16, now: u64) -> Option<Delivery> {
    let mut errors_until = 0;
    let mut transactions_until = 0;
    if let Some(header) = header.filter(|h| h.len() <= 16384) {
        for entry in header.split(',').take(64) {
            let mut fields = entry.trim().split(':');
            let Some(seconds) = fields
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0)
            else {
                continue;
            };
            let until = now.saturating_add(seconds.ceil().min(u32::MAX as f64) as u64);
            let Some(categories) = fields.next() else { continue };
            if categories.is_empty() {
                errors_until = errors_until.max(until);
                transactions_until = transactions_until.max(until);
            }
            for category in categories.split(';') {
                match category {
                    "error" => errors_until = errors_until.max(until),
                    "transaction" | "span" => transactions_until = transactions_until.max(until),
                    _ => {}
                }
            }
        }
    } else if status == 429 {
        let delay = retry.and_then(|r| r.parse::<u64>().ok()).unwrap_or(60);
        errors_until = now.saturating_add(delay);
        transactions_until = errors_until;
    }
    if errors_until > 0 || transactions_until > 0 {
        Some(Delivery::Limited {
            errors_until,
            transactions_until,
        })
    } else if status == 429 {
        Some(Delivery::Drop)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoint_requires_build_region_and_never_accepts_redirect_recipient() {
        let key = "a".repeat(32);
        assert!(Endpoint::parse(&format!("https://{key}@o1.ingest.de.sentry.io/2"), Region::Eu, 3).is_ok());
        let endpoint = Endpoint::parse(&format!("https://{key}@o1.ingest.de.sentry.io/2"), Region::Eu, 3).unwrap();
        let debug = format!("{endpoint:?}");
        assert!(!debug.contains(&key));
        assert!(!debug.contains("sentry.io"));
        for dsn in [
            format!("http://{key}@o1.ingest.de.sentry.io/2"),
            format!("https://{key}@o1.ingest.us.sentry.io/2"),
            format!("https://{key}@o1.ingest.de.sentry.io.attacker.invalid/2"),
            format!("https://{key}:secret@o1.ingest.de.sentry.io/2"),
        ] {
            assert!(Endpoint::parse(&dsn, Region::Eu, 3).is_err());
        }
    }
    #[test]
    fn category_limits_do_not_relabel_or_backlog() {
        assert_eq!(
            limits(
                Some("60:error:organization,120:transaction:organization"),
                None,
                200,
                100
            ),
            Some(Delivery::Limited {
                errors_until: 160,
                transactions_until: 220
            })
        );
        assert_eq!(
            limits(None, Some("90"), 429, 100),
            Some(Delivery::Limited {
                errors_until: 190,
                transactions_until: 190
            })
        );
        assert_eq!(limits(Some("60:unknown:organization"), None, 200, 100), None);
    }
    #[tokio::test]
    async fn local_http_mock_gets_only_closed_envelope_and_no_redirect_follow() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let endpoint = Endpoint {
            url: Url::parse(&format!("http://{address}/api/1/envelope/")).unwrap(),
            public_key: "a".repeat(32),
            region: Region::Eu,
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 32768];
            let mut n = 0;
            loop {
                n += stream.read(&mut bytes[n..]).await.unwrap();
                if bytes[..n].windows(2).any(|b| b == b"}\n") && bytes[..n].windows(11).any(|b| b == b"\"exception\"") {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&bytes[..n]);
            assert!(text.contains("application/x-sentry-envelope"));
            assert!(text.contains(r#""infer_ip":"never""#));
            assert!(!text.contains("cookie:"));
            assert!(!text.contains("user\""));
            stream
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/forbidden\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let report = AdmittedReport::new(
            Report::SafeCrash(SafeCrash::empty(
                BuildContext {
                    app_version: AppVersion {
                        major: 1,
                        minor: 0,
                        patch: 0,
                        build: 1,
                    },
                    os_version_class: OsVersionClass::Unknown,
                },
                Component::Agent,
                CrashKind::RustPanic,
                ExceptionClass::Panic,
            )),
            now(),
        )
        .unwrap();
        let submission = Submission {
            bytes: wire::encode(&report).unwrap(),
            report,
            generation: 1,
        };
        assert_eq!(Https::new(endpoint).unwrap().send(&submission).await, Delivery::Drop);
        server.await.unwrap();
    }
}

#[cfg(test)]
mod consent_network_tests {
    use super::*;
    #[tokio::test]
    async fn default_off_and_revoked_queues_make_no_connection_to_local_mock() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Endpoint {
            url: Url::parse(&format!("http://{}/api/1/envelope/", listener.local_addr().unwrap())).unwrap(),
            public_key: "a".repeat(32),
            region: Region::Eu,
        };
        let https = Https::new(endpoint).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut store = store::Store::open(dir.path(), ReportConsent::default(), now()).unwrap();
        let crash = Report::SafeCrash(SafeCrash::empty(
            BuildContext {
                app_version: AppVersion {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    build: 1,
                },
                os_version_class: OsVersionClass::Unknown,
            },
            Component::Agent,
            CrashKind::RustPanic,
            ExceptionClass::Panic,
        ));
        assert_eq!(store.observe(0, crash.clone(), now(), now()), Err(Error::Disabled));
        if let Ok(Some(submission)) = store.prepare(0, now()) {
            https.send(&submission).await;
        }
        store
            .set_consent(ReportConsent {
                choice: Choice::Granted,
                policy_version: 3,
                generation: 1,
            })
            .unwrap();
        store.observe(1, crash, now(), now()).unwrap();
        store
            .set_consent(ReportConsent {
                choice: Choice::Denied,
                policy_version: 3,
                generation: 2,
            })
            .unwrap();
        if let Ok(Some(submission)) = store.prepare(1, now()) {
            https.send(&submission).await;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
        assert!(!store.has_work());
    }
}
