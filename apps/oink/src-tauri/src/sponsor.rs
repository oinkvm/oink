//! One anonymous, bounded download per UI process; no disk cache or WebView network access.
use base64::Engine;
use serde::Serialize;
use std::time::Duration;
use tokio::sync::watch;

const URL: &str = "https://oink.hachiwii.com/sponsor/assets/wechat_qr_raw";
const MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SponsorImage {
    mime_type: String,
    base64: String,
}

pub(crate) struct SponsorQr(watch::Receiver<Option<Option<SponsorImage>>>);

impl SponsorQr {
    #[cfg(any(target_os = "macos", windows))]
    pub(crate) fn start() -> Self {
        // The outer Option distinguishes pending from a completed failure. All
        // windows (including recreated WebViews) share this one in-memory result.
        let (complete, result) = watch::channel(None);
        tauri::async_runtime::spawn(async move {
            let image = match client() {
                Ok(client) => download(&client, URL).await,
                Err(_) => None,
            };
            complete.send_replace(Some(image));
        });
        Self(result)
    }

    async fn image(&self) -> Option<SponsorImage> {
        let mut result = self.0.clone();
        result.wait_for(Option::is_some).await.ok()?.as_ref().cloned().flatten()
    }
}

fn client() -> Result<reqwest::Client, reqwest::Error> {
    client_builder().build()
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(7))
        // Cookie storage and automatic Referer are disabled; no identifiers,
        // custom headers, client certificates or persistent HTTP cache are used.
        .referer(false)
}

async fn download(client: &reqwest::Client, url: &str) -> Option<SponsorImage> {
    let mut response = client.get(url).send().await.ok()?;
    if !response.status().is_success() || response.content_length().is_some_and(|size| size > MAX_BYTES as u64) {
        return None;
    }
    let mime_type = response.headers().get(reqwest::header::CONTENT_TYPE)?.to_str().ok()?;
    let mime_type = mime_type.split(';').next()?.trim().to_ascii_lowercase();
    if !matches!(mime_type.as_str(), "image/png" | "image/jpeg") {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > MAX_BYTES - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return None;
    }
    Some(SponsorImage {
        mime_type,
        base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

#[cfg(any(target_os = "macos", windows))]
#[tauri::command]
pub(crate) async fn shell_sponsor_qr(state: tauri::State<'_, SponsorQr>) -> Result<Option<SponsorImage>, ()> {
    Ok(state.image().await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve(response: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/qr", listener.local_addr().unwrap());
        let request = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let size = socket.read(&mut request).await.unwrap();
            let _ = socket.write_all(&response).await;
            String::from_utf8(request[..size].to_vec()).unwrap()
        });
        (url, request)
    }

    // Loopback HTTP only in this fixture; the product client rejects HTTP.
    fn test_client() -> reqwest::Client {
        client_builder()
            .https_only(false)
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn downloads_only_plain_get_and_hands_off_bytes() {
        let (url, request) = serve(
            b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 3\r\nConnection: close\r\n\r\nPNG".to_vec(),
        )
        .await;
        let image = download(&test_client(), &url).await.unwrap();
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.base64, "UE5H");
        let request = request.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /qr http/1.1\r\n"));
        for header in request.lines().skip(1).filter(|line| !line.is_empty()) {
            assert!(
                header.starts_with("host:") || header == "accept: */*",
                "unexpected header: {header}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_status_mime_empty_body_and_declared_oversize() {
        for response in [
            "HTTP/1.1 500 Error\r\nContent-Type: image/png\r\nContent-Length: 0\r\n\r\n".to_owned(),
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 3\r\n\r\nPNG".to_owned(),
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nPNG".to_owned(),
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 0\r\n\r\n".to_owned(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\n\r\n",
                MAX_BYTES + 1
            ),
            "HTTP/1.1 302 Found\r\nLocation: https://example.invalid/qr\r\nContent-Length: 0\r\n\r\n".to_owned(),
        ] {
            let (url, request) = serve(response.into_bytes()).await;
            assert!(download(&test_client(), &url).await.is_none());
            request.await.unwrap();
        }
    }

    #[tokio::test]
    async fn caps_body_without_content_length() {
        let mut response = b"HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nConnection: close\r\n\r\n".to_vec();
        response.resize(response.len() + MAX_BYTES + 1, 0);
        let (url, request) = serve(response).await;
        assert!(download(&test_client(), &url).await.is_none());
        request.await.unwrap();
    }

    #[tokio::test]
    async fn production_client_rejects_http() {
        assert!(
            download(&client().unwrap(), "http://example.invalid/qr")
                .await
                .is_none()
        );
        assert!(URL.starts_with("https://oink.hachiwii.com/"));
    }

    #[tokio::test]
    async fn stalled_request_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/qr", listener.local_addr().unwrap());
        let client = client_builder()
            .https_only(false)
            .no_proxy()
            .timeout(Duration::from_millis(20))
            .build()
            .unwrap();
        assert!(download(&client, &url).await.is_none());
    }

    #[tokio::test]
    async fn all_windows_share_one_completed_result_including_failure() {
        let (complete, result) = watch::channel(None);
        let qr = SponsorQr(result);
        let first = qr.image();
        let second = qr.image();
        complete.send_replace(Some(Some(SponsorImage {
            mime_type: "image/png".into(),
            base64: "UE5H".into(),
        })));
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap().base64, second.unwrap().base64);
        assert_eq!(qr.image().await.unwrap().base64, "UE5H");
        complete.send_replace(Some(None));
        assert!(qr.image().await.is_none());
    }
}
