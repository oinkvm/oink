//! One anonymous manifest fetch per UI process. Only resolved data URIs reach the WebView.
use base64::Engine;
use image::ImageDecoder;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    time::Duration,
};
use tokio::sync::watch;

const URL: &str = "https://oink.hachiwii.com/sponsor/list.json";
const MAX_MANIFEST: usize = 64 * 1024;
const MAX_ITEMS: usize = 8;
const MAX_ICON: usize = 256 * 1024;
const MAX_QR: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalizedText {
    default: String,
    #[serde(default)]
    locales: HashMap<String, String>,
}

impl LocalizedText {
    fn valid(&self, max: usize) -> bool {
        valid_text(&self.default, max)
            && self
                .locales
                .iter()
                .all(|(locale, text)| matches!(locale.as_str(), "zh" | "en" | "ja" | "ko") && valid_text(text, max))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub(crate) enum Action {
    Link {
        url: String,
    },
    Qr {
        image: String,
        caption: Option<LocalizedText>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SponsorItem {
    id: String,
    name: LocalizedText,
    icon: String,
    action: Action,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    items: Vec<serde_json::Value>,
}

pub(crate) struct SponsorList(watch::Receiver<Option<Option<Vec<SponsorItem>>>>);

impl SponsorList {
    #[cfg(any(target_os = "macos", windows))]
    pub(crate) fn start() -> Self {
        // Outer Option distinguishes pending from failure. Recreated windows share
        // the same startup request and in-memory result; nothing is saved to disk.
        let (complete, result) = watch::channel(None);
        tauri::async_runtime::spawn(async move {
            let items = match client() {
                Ok(client) => tokio::time::timeout(Duration::from_secs(30), load(&client, URL))
                    .await
                    .ok()
                    .flatten(),
                Err(_) => None,
            };
            complete.send_replace(Some(items));
        });
        Self(result)
    }

    async fn items(&self) -> Option<Vec<SponsorItem>> {
        let mut result = self.0.clone();
        result.wait_for(Option::is_some).await.ok()?.as_ref().cloned().flatten()
    }
}

fn valid_text(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.chars().count() <= max && !text.chars().any(char::is_control)
}

fn parsed_url(value: &str) -> Option<Url> {
    if value.len() > 2048 {
        return None;
    }
    let url = Url::parse(value).ok()?;
    (url.username().is_empty() && url.password().is_none() && url.fragment().is_none()).then_some(url)
}

fn image_url(value: &str, origin: &Url) -> bool {
    // The product origin always comes from the fixed HTTPS manifest URL. Tests
    // use a loopback HTTP origin without weakening the production client policy.
    parsed_url(value).is_some_and(|url| url.origin() == origin.origin())
}

fn link_url(value: &str) -> bool {
    parsed_url(value).is_some_and(|url| url.scheme() == "https" && url.host_str().is_some())
}

fn manifest(bytes: &[u8], origin: &Url) -> Option<Vec<SponsorItem>> {
    if bytes.len() > MAX_MANIFEST {
        return None;
    }
    let manifest: Manifest = serde_json::from_slice(bytes).ok()?;
    if manifest.version != 1 || manifest.items.is_empty() || manifest.items.len() > MAX_ITEMS {
        return None;
    }
    let mut ids = HashSet::new();
    Some(
        manifest
            .items
            .into_iter()
            .filter_map(|value| {
                let item: SponsorItem = serde_json::from_value(value).ok()?;
                let valid = !item.id.is_empty()
                    && item.id.len() <= 64
                    && item
                        .id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    && item.name.valid(100)
                    && image_url(&item.icon, origin)
                    && match &item.action {
                        Action::Link { url } => link_url(url),
                        Action::Qr { image, caption } => {
                            image_url(image, origin) && caption.as_ref().is_none_or(|text| text.valid(240))
                        }
                    };
                (valid && ids.insert(item.id.clone())).then_some(item)
            })
            .collect(),
    )
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
        // No cookie store, Referer, identifiers, custom headers, client certs,
        // retry, persistent HTTP cache, or WebView network access.
        .referer(false)
}

async fn download(client: &reqwest::Client, url: &str, max: usize, accepted: &[&str]) -> Option<(String, Vec<u8>)> {
    let mut response = client.get(url).send().await.ok()?;
    if !response.status().is_success() || response.content_length().is_some_and(|size| size > max as u64) {
        return None;
    }
    let mime = response.headers().get(reqwest::header::CONTENT_TYPE)?.to_str().ok()?;
    let mime = mime.split(';').next()?.trim().to_ascii_lowercase();
    if !accepted.contains(&mime.as_str()) {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > max - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    (!bytes.is_empty()).then_some((mime, bytes))
}

async fn image(client: &reqwest::Client, url: &str, icon: bool) -> Option<String> {
    let (mime, bytes) = download(
        client,
        url,
        if icon { MAX_ICON } else { MAX_QR },
        &["image/png", "image/jpeg"],
    )
    .await?;
    let format = match mime.as_str() {
        "image/png" => image::ImageFormat::Png,
        "image/jpeg" => image::ImageFormat::Jpeg,
        _ => return None,
    };
    let (min, max) = if icon { (16, 512) } else { (64, 2048) };
    // Apply allocation limits even while reading image metadata, then check
    // dimensions before allocating pixels and fully decode to reject corruption.
    let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(max);
    limits.max_image_height = Some(max);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let decoder = reader.into_decoder().ok()?;
    let (width, height) = decoder.dimensions();
    if !(min..=max).contains(&width) || !(min..=max).contains(&height) {
        return None;
    }
    image::DynamicImage::from_decoder(decoder).ok()?;
    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

async fn load(client: &reqwest::Client, url: &str) -> Option<Vec<SponsorItem>> {
    let origin = Url::parse(url).ok()?;
    let (_, bytes) = download(client, url, MAX_MANIFEST, &["application/json"]).await?;
    let items = manifest(&bytes, &origin)?;
    let mut resolved = Vec::new();
    for mut item in items {
        let Some(icon) = image(client, &item.icon, true).await else {
            continue;
        };
        item.icon = icon;
        if let Action::Qr { image: src, .. } = &mut item.action {
            let Some(qr) = image(client, src, false).await else {
                continue;
            };
            *src = qr;
        }
        resolved.push(item);
    }
    (!resolved.is_empty()).then_some(resolved)
}

#[cfg(any(target_os = "macos", windows))]
#[tauri::command]
pub(crate) async fn shell_sponsor_list(state: tauri::State<'_, SponsorList>) -> Result<Option<Vec<SponsorItem>>, ()> {
    Ok(state.items().await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn origin() -> Url {
        Url::parse(URL).unwrap()
    }
    fn link(id: &str) -> Value {
        json!({"id":id,"name":{"default":"Support", "locales":{"zh":"赞助"}},
            "icon":"https://oink.hachiwii.com/sponsor/assets/icon.png",
            "action":{"type":"link","url":"https://ko-fi.com/hachiwii"}})
    }
    fn parse(items: Vec<Value>) -> Option<Vec<SponsorItem>> {
        manifest(
            &serde_json::to_vec(&json!({"version":1,"items":items})).unwrap(),
            &origin(),
        )
    }
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(width, height)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }
    fn response(mime: &str, bytes: &[u8]) -> Vec<u8> {
        let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nSet-Cookie: ignored=1\r\nConnection: close\r\n\r\n", bytes.len()).into_bytes();
        response.extend_from_slice(bytes);
        response
    }
    async fn serve(responses: Vec<Vec<u8>>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/sponsor/list.json", listener.local_addr().unwrap());
        let requests = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 4096];
                let size = socket.read(&mut request).await.unwrap();
                let _ = socket.write_all(&response).await;
                requests.push(String::from_utf8(request[..size].to_vec()).unwrap());
            }
            requests
        });
        (url, requests)
    }
    fn test_client() -> reqwest::Client {
        client_builder()
            .https_only(false)
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap()
    }

    #[test]
    fn documented_example_is_a_valid_manifest() {
        let doc = include_str!("../../../../docs/design/sponsor.md");
        let example = doc.split("```json\n").nth(1).unwrap().split("```").next().unwrap();
        assert_eq!(manifest(example.as_bytes(), &origin()).unwrap().len(), 2);
    }

    #[test]
    fn rejects_invalid_manifest_version_shape_size_and_count() {
        for value in [
            json!({"version":2,"items":[link("a")]}),
            json!({"version":1,"items":{}}),
            json!({"version":1,"items":[]}),
            json!({"version":1,"items":[link("a")],"extra":true}),
            json!({"version":1,"items":vec![link("a"); MAX_ITEMS + 1]}),
        ] {
            assert!(manifest(&serde_json::to_vec(&value).unwrap(), &origin()).is_none());
        }
        assert!(manifest(b"not json", &origin()).is_none());
        assert!(manifest(&vec![b' '; MAX_MANIFEST + 1], &origin()).is_none());
    }

    #[test]
    fn preserves_order_locales_and_both_action_types() {
        let mut qr = link("qr");
        qr["action"] = json!({"type":"qr","image":"https://oink.hachiwii.com/sponsor/assets/qr.png",
            "caption":{"default":"Scan", "locales":{"en":"Scan me","ja":"スキャン","ko":"스캔","zh":"扫一扫"}}});
        let items = parse(vec![link("link"), qr]).unwrap();
        assert_eq!(
            items.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
            ["link", "qr"]
        );
        assert_eq!(items[0].name.locales["zh"], "赞助");
        assert!(matches!(&items[1].action, Action::Qr { caption: Some(text), .. } if text.locales.len() == 4));
    }

    #[test]
    fn drops_invalid_items_individually_including_duplicates_and_ambiguous_actions() {
        let mut invalid = Vec::new();
        for (field, value) in [
            ("id", json!("bad id")),
            ("id", json!("x".repeat(65))),
            ("name", json!({"default":""})),
            ("name", json!({"default":"x".repeat(101)})),
            ("name", json!({"default":"x\n"})),
            ("name", json!({"default":"X","locales":{"fr":"Y"}})),
            ("icon", json!("https://other.example/icon.png")),
            ("action", json!({"type":"unknown"})),
            (
                "action",
                json!({"type":"link","url":"https://example.org","image":"https://oink.hachiwii.com/a.png"}),
            ),
            (
                "action",
                json!({"type":"qr","image":"https://oink.hachiwii.com/a.png","caption":{"default":"x".repeat(241)}}),
            ),
        ] {
            let mut item = link("invalid");
            item[field] = value;
            invalid.push(item);
        }
        invalid.push(json!(null));
        for bad in invalid {
            let items = parse(vec![link("first"), bad, link("last"), link("first")]).unwrap();
            assert_eq!(
                items.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
                ["first", "last"]
            );
        }
        assert!(parse(vec![json!(null)]).unwrap().is_empty());
    }

    #[test]
    fn enforces_image_origin_credentials_and_url_bounds() {
        for bad in [
            "http://oink.hachiwii.com/i.png",
            "https://oink.hachiwii.com:444/i.png",
            "https://oink.hachiwii.com.evil/i.png",
            "https://evil.example/i.png",
            "data:image/png;base64,AA==",
            "/sponsor/icon.png",
            "https://user@oink.hachiwii.com/i.png",
            "https://oink.hachiwii.com/i.png#fragment",
        ] {
            assert!(!image_url(bad, &origin()), "{bad}");
            let mut item = link("bad");
            item["action"] = json!({"type":"qr","image":bad});
            assert!(parse(vec![item]).unwrap().is_empty());
        }
        assert!(image_url("https://oink.hachiwii.com:443/sponsor/icon.png", &origin()));
        assert!(!image_url(
            &format!("https://oink.hachiwii.com/{}", "x".repeat(2048)),
            &origin()
        ));
        for bad in [
            "http://example.org",
            "javascript:alert(1)",
            "file:///tmp/a",
            "https://user:password@example.org",
        ] {
            let mut item = link("bad");
            item["action"] = json!({"type":"link","url":bad});
            assert!(parse(vec![item]).unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn resolves_manifest_in_order_drops_failed_images_and_sends_only_anonymous_gets() {
        // Bind first so the fixture manifest can reference the exact loopback origin.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let mut first = link("first");
        first["icon"] = json!(format!("{root}/first.png"));
        let mut bad = link("bad");
        bad["icon"] = json!(format!("{root}/bad.png"));
        let mut qr = link("qr");
        qr["icon"] = json!(format!("{root}/icon.png"));
        qr["action"] = json!({"type":"qr","image":format!("{root}/qr.png")});
        let body = serde_json::to_vec(&json!({"version":1,"items":[first, null, bad, qr]})).unwrap();
        let responses = vec![
            response("application/json; charset=utf-8", &body),
            response("image/png", &png(20, 20)),
            response("image/png", b"corrupt"),
            response("image/png", &png(32, 32)),
            response("image/png", &png(64, 64)),
        ];
        let requests = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 4096];
                let count = socket.read(&mut buf).await.unwrap();
                requests.push(String::from_utf8(buf[..count].to_vec()).unwrap());
                socket.write_all(&response).await.unwrap();
            }
            requests
        });
        let items = load(&test_client(), &format!("{root}/sponsor/list.json"))
            .await
            .unwrap();
        assert_eq!(
            items.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
            ["first", "qr"]
        );
        assert!(items.iter().all(|item| item.icon.starts_with("data:image/png;base64,")));
        assert!(matches!(&items[1].action, Action::Qr { image, .. } if image.starts_with("data:image/png;base64,")));
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 5);
        for request in requests {
            assert!(request.starts_with("GET /"));
            for header in request
                .to_ascii_lowercase()
                .lines()
                .skip(1)
                .filter(|line| !line.is_empty())
            {
                assert!(
                    header.starts_with("host:") || header == "accept: */*",
                    "unexpected: {header}"
                );
            }
        }
    }

    #[tokio::test]
    async fn accepts_png_and_jpeg_but_rejects_corruption_mime_mismatch_and_pixel_bounds() {
        let mut jpeg = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(20, 20)
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let (url, requests) = serve(vec![response("image/jpeg", &jpeg.into_inner())]).await;
        assert!(
            image(&test_client(), &url, true)
                .await
                .unwrap()
                .starts_with("data:image/jpeg;base64,")
        );
        requests.await.unwrap();
        for (mime, bytes, icon, expected) in [
            ("image/png", png(20, 20), true, true),
            ("image/png", png(64, 64), false, true),
            ("image/png", png(15, 20), true, false),
            ("image/png", png(513, 20), true, false),
            ("image/png", png(63, 64), false, false),
            ("image/png", png(2049, 64), false, false),
            ("image/jpeg", png(64, 64), false, false),
            ("image/png", b"PNG".to_vec(), true, false),
            ("image/svg+xml", b"<svg/>".to_vec(), true, false),
            ("image/webp", b"WEBP".to_vec(), true, false),
        ] {
            let (url, requests) = serve(vec![response(mime, &bytes)]).await;
            assert_eq!(image(&test_client(), &url, icon).await.is_some(), expected);
            requests.await.unwrap();
        }
    }

    #[tokio::test]
    async fn rejects_status_mime_empty_body_declared_and_streamed_oversize_without_redirects() {
        let mut streamed = b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n".to_vec();
        streamed.resize(streamed.len() + MAX_ICON + 1, 0);
        for response in [
            b"HTTP/1.1 500 Error\r\nContent-Length: 0\r\n\r\n".to_vec(),
            response("text/html", b"PNG"),
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nPNG".to_vec(),
            response("image/png", b""),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\n\r\n",
                MAX_ICON + 1
            )
            .into_bytes(),
            b"HTTP/1.1 302 Found\r\nLocation: https://example.invalid/qr\r\nContent-Length: 0\r\n\r\n".to_vec(),
            streamed,
        ] {
            let (url, requests) = serve(vec![response]).await;
            assert!(download(&test_client(), &url, MAX_ICON, &["image/png"]).await.is_none());
            assert_eq!(requests.await.unwrap().len(), 1);
        }
        let (url, requests) = serve(vec![response("application/json", b"{}")]).await;
        assert!(load(&test_client(), &url).await.is_none());
        requests.await.unwrap();
    }

    #[tokio::test]
    async fn production_rejects_http_and_stalled_requests_time_out() {
        assert!(
            load(&client().unwrap(), "http://example.invalid/list.json")
                .await
                .is_none()
        );
        assert!(URL.starts_with("https://oink.hachiwii.com/sponsor/"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/list.json", listener.local_addr().unwrap());
        let client = client_builder()
            .https_only(false)
            .no_proxy()
            .timeout(Duration::from_millis(20))
            .build()
            .unwrap();
        assert!(load(&client, &url).await.is_none());
    }

    #[tokio::test]
    async fn windows_share_one_completed_result_including_failure() {
        let (complete, result) = watch::channel(None);
        let list = SponsorList(result);
        let first = list.items();
        let second = list.items();
        complete.send_replace(Some(parse(vec![link("a")])));
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap()[0].id, second.unwrap()[0].id);
        assert_eq!(list.items().await.unwrap()[0].id, "a");
        complete.send_replace(Some(None));
        assert!(list.items().await.is_none());
    }
}
