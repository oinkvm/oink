//! 文本类格式的转换（纯函数）：`HTML Format`（CF_HTML）头部与片段偏移、换行符。
//!
//! CF_HTML 是 UTF-8 文本，开头是 `Key:Value` 头部，偏移都是**字节偏移**：
//! ```text
//! Version:0.9
//! StartHTML:0000000105
//! EndHTML:0000000199
//! StartFragment:0000000141
//! EndFragment:0000000163
//! <html><body>
//! <!--StartFragment-->...<!--EndFragment-->
//! </body></html>
//! ```
//! 线上格式 `ClipData::Html` 是片段本身（StartFragment..EndFragment）。

const START_MARKER: &str = "<!--StartFragment-->";
const END_MARKER: &str = "<!--EndFragment-->";

/// 从 CF_HTML 数据中取出片段。偏移不可用时依次退回 StartHTML..EndHTML 中的标记、全文中的标记、全文。
pub(crate) fn decode_cf_html(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let data = &bytes[..end];
    let header = parse_header(data);
    let slice = |s: Option<i64>, e: Option<i64>| -> Option<&[u8]> {
        let (s, e) = (usize::try_from(s?).ok()?, usize::try_from(e?).ok()?);
        (s <= e && e <= data.len()).then(|| &data[s..e])
    };
    if let Some(s) = slice(header.start_fragment, header.end_fragment).and_then(|f| std::str::from_utf8(f).ok()) {
        return Some(s.to_string());
    }
    let html = slice(header.start_html, header.end_html)
        .and_then(|b| std::str::from_utf8(b).ok())
        .map(str::to_string)
        .or_else(|| std::str::from_utf8(&data[header.body_offset..]).ok().map(str::to_owned))?;
    Some(between_markers(&html).unwrap_or(html))
}

fn between_markers(html: &str) -> Option<String> {
    let s = html.find(START_MARKER)? + START_MARKER.len();
    let e = html[s..].find(END_MARKER)? + s;
    Some(html[s..e].to_string())
}

#[derive(Debug, Default)]
struct Header {
    start_html: Option<i64>,
    end_html: Option<i64>,
    start_fragment: Option<i64>,
    end_fragment: Option<i64>,
    /// 头部之后第一个字节的位置。
    body_offset: usize,
}

fn parse_header(data: &[u8]) -> Header {
    let mut h = Header::default();
    let mut pos = 0;
    while pos < data.len() {
        let line_end = data[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(data.len(), |i| pos + i + 1);
        let line = String::from_utf8_lossy(&data[pos..line_end]);
        let line = line.trim_end_matches(['\r', '\n']);
        let Some((k, v)) = line.split_once(':') else { break };
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric()) {
            break;
        }
        if !matches!(
            k,
            "Version"
                | "StartHTML"
                | "EndHTML"
                | "StartFragment"
                | "EndFragment"
                | "StartSelection"
                | "EndSelection"
                | "SourceURL"
        ) {
            break;
        }
        let num = v.trim().parse::<i64>().ok();
        match k {
            "StartHTML" => h.start_html = num,
            "EndHTML" => h.end_html = num,
            "StartFragment" => h.start_fragment = num,
            "EndFragment" => h.end_fragment = num,
            _ => {}
        }
        pos = line_end;
    }
    h.body_offset = pos;
    h
}

/// 把片段（或完整 HTML 文档）封装成 CF_HTML（UTF-8，末尾带 NUL）。
///
/// 输入是完整文档（含 `<body`）时，把片段标记插在 body 内；否则用 `<html><body>` 包起来。
pub(crate) fn encode_cf_html(html: &str) -> Vec<u8> {
    let (pre, frag, post) = split_document(html);
    // 数字固定 10 位，头部长度固定
    let header_len = format!(
        "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
        0, 0, 0, 0
    )
    .len();
    let start_html = header_len;
    let start_fragment = start_html + pre.len() + START_MARKER.len();
    let end_fragment = start_fragment + frag.len();
    let end_html = end_fragment + END_MARKER.len() + post.len();
    let mut out = format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n"
    );
    debug_assert_eq!(out.len(), header_len);
    out.push_str(&pre);
    out.push_str(START_MARKER);
    out.push_str(frag);
    out.push_str(END_MARKER);
    out.push_str(&post);
    let mut bytes = out.into_bytes();
    bytes.push(0);
    bytes
}

/// 返回（片段前的文档部分，片段，片段后的文档部分）。
fn split_document(html: &str) -> (String, &str, String) {
    let lower = html.to_ascii_lowercase();
    let body_start = lower
        .find("<body")
        .and_then(|body| lower[body..].find('>').map(|gt| body + gt + 1));
    if let Some(s) = body_start {
        let e = lower[s..].rfind("</body").map_or(html.len(), |i| s + i);
        // 已有标记时只取标记内
        let inner = &html[s..e];
        if let (Some(ms), Some(me)) = (inner.find(START_MARKER), inner.rfind(END_MARKER))
            && ms + START_MARKER.len() <= me
        {
            let fs = s + ms + START_MARKER.len();
            let fe = s + me;
            return (
                html[..s + ms].to_string(),
                &html[fs..fe],
                html[fe + END_MARKER.len()..].to_string(),
            );
        }
        return (html[..s].to_string(), inner, html[e..].to_string());
    }
    (
        "<html>\r\n<body>\r\n".to_string(),
        html,
        "\r\n</body>\r\n</html>".to_string(),
    )
}

/// 线上文本统一用 LF；读取 Windows 剪贴板时 CRLF → LF。
pub(crate) fn crlf_to_lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// 写入 Windows 剪贴板时 LF → CRLF（已有的 CRLF 不重复转换，单独的 CR 保留）。
pub(crate) fn lf_to_crlf(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 32);
    let mut prev = '\0';
    for c in s.chars() {
        if c == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(c);
        prev = c;
    }
    out
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn encode_then_decode_roundtrip() {
        for frag in ["<b>粗体</b> and <i>é</i>", "", "plain"] {
            let cf = encode_cf_html(frag);
            assert_eq!(*cf.last().unwrap(), 0);
            assert_eq!(decode_cf_html(&cf).as_deref(), Some(frag));
            // 偏移是字节偏移
            let text = std::str::from_utf8(&cf[..cf.len() - 1]).unwrap();
            let h = parse_header(text.as_bytes());
            let (s, e) = (h.start_fragment.unwrap() as usize, h.end_fragment.unwrap() as usize);
            assert_eq!(&text[s..e], frag);
            assert_eq!(h.end_html.unwrap() as usize, text.len());
            assert!(text[h.start_html.unwrap() as usize..].starts_with("<html>"));
        }
    }

    #[test]
    fn encode_full_document_keeps_head() {
        let doc = "<html><head><style>p{color:red}</style></head><body class=x><p>hi</p></body></html>";
        let cf = encode_cf_html(doc);
        let text = std::str::from_utf8(&cf[..cf.len() - 1]).unwrap();
        assert!(text.contains("<style>p{color:red}</style>"));
        assert!(text.contains("<body class=x><!--StartFragment--><p>hi</p><!--EndFragment--></body>"));
        assert_eq!(decode_cf_html(&cf).as_deref(), Some("<p>hi</p>"));
    }

    #[test]
    fn decode_chrome_style() {
        // Chrome 的写法：StartHTML/EndHTML 可能是 -1 以外的正常值，末尾带 NUL
        let body = "<html>\r\n<body>\r\n<!--StartFragment--><span>x</span><!--EndFragment-->\r\n</body>\r\n</html>";
        let head_len = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\nSourceURL:https://example.com/\r\n".len();
        let sf = head_len + body.find("<span>").unwrap();
        let ef = head_len + body.find("<!--EndFragment").unwrap();
        let s = format!(
            "Version:0.9\r\nStartHTML:{head_len:010}\r\nEndHTML:{:010}\r\nStartFragment:{sf:010}\r\nEndFragment:{ef:010}\r\nSourceURL:https://example.com/\r\n{body}\0",
            head_len + body.len()
        );
        assert_eq!(decode_cf_html(s.as_bytes()).as_deref(), Some("<span>x</span>"));
    }

    #[test]
    fn decode_with_bad_offsets_falls_back_to_markers() {
        let s = "Version:0.9\r\nStartHTML:-1\r\nEndHTML:-1\r\nStartFragment:9999\r\nEndFragment:99999\r\n<html><body><!--StartFragment-->ok<!--EndFragment--></body></html>";
        assert_eq!(decode_cf_html(s.as_bytes()).as_deref(), Some("ok"));
    }

    #[test]
    fn decode_without_header() {
        assert_eq!(decode_cf_html(b"<p>raw</p>").as_deref(), Some("<p>raw</p>"));
    }

    #[test]
    fn malformed_utf8_is_unavailable_and_colon_text_is_not_a_header() {
        assert!(decode_cf_html(b"<p>\xff</p>").is_none());
        assert_eq!(decode_cf_html(b"hello:world").as_deref(), Some("hello:world"));
    }

    #[test]
    fn newline_conversion() {
        assert_eq!(crlf_to_lf("a\r\nb\nc\r"), "a\nb\nc\r");
        assert_eq!(lf_to_crlf("a\nb\r\nc\rd"), "a\r\nb\r\nc\rd");
        assert_eq!(lf_to_crlf(&crlf_to_lf("x\r\ny\r\n")), "x\r\ny\r\n");
    }
}
