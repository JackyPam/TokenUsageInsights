//! 將 `export-all` 的 JSON 上傳至 Azure Blob Storage（以 SAS URL 授權）。
//!
//! SAS 的 `sig` 等同密碼：任何對外輸出（成功訊息、錯誤訊息）都不得包含 query string。

use chrono::{DateTime, TimeZone};
use reqwest::{StatusCode, Url};
use std::time::Duration;

/// 完整匯出可能很大，給予較長的整體逾時。
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const FALLBACK_COMPUTER_NAME: &str = "unknown-host";

/// `--out` 是否為 URL（`http://` 或 `https://`）；其餘一律視為本機檔案路徑。
pub(crate) fn is_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

/// 預設檔名：`yyyy-MM-dd_HH-mm-ss_<電腦名稱>.json`。
pub(crate) fn export_file_name<Tz>(now: &DateTime<Tz>, computer_name: &str) -> String
where
    Tz: TimeZone,
    Tz::Offset: std::fmt::Display,
{
    format!(
        "{}_{}.json",
        now.format("%Y-%m-%d_%H-%M-%S"),
        sanitize_computer_name(computer_name)
    )
}

/// 保留字母、數字與 `-_.`（含中文電腦名稱），其餘字元換成 `-`，避免檔名出現路徑分隔或空白。
fn sanitize_computer_name(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        FALLBACK_COMPUTER_NAME.to_string()
    } else {
        cleaned
    }
}

pub(crate) fn computer_name() -> String {
    let non_empty = |value: String| (!value.trim().is_empty()).then_some(value);
    std::env::var("COMPUTERNAME")
        .ok()
        .and_then(non_empty)
        .or_else(os_hostname)
        .or_else(|| std::env::var("HOSTNAME").ok().and_then(non_empty))
        .map(|name| sanitize_computer_name(&name))
        .unwrap_or_else(|| FALLBACK_COMPUTER_NAME.to_string())
}

#[cfg(unix)]
fn os_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: 傳入的長度等於 buf 的實際長度，gethostname 不會寫出界。
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(not(unix))]
fn os_hostname() -> Option<String> {
    // Windows 由 COMPUTERNAME 環境變數提供電腦名稱。
    None
}

/// 由 SAS URL 組出實際上傳的 Blob URL，query string（含簽章）原樣保留。
///
/// - 容器 SAS（`https://<account>.blob.core.windows.net/<container>?<SAS>`）：附加預設檔名。
/// - Blob SAS（`sr=b`）：該 SAS 只授權單一 Blob，直接使用 URL 內指定的 Blob。
pub(crate) fn build_blob_url(sas_url: &str, file_name: &str) -> Result<Url, String> {
    let mut url = Url::parse(sas_url).map_err(|e| format!("無法解析 SAS URL: {e}"))?;
    if url.scheme() != "https" {
        return Err("SAS URL 必須使用 HTTPS".to_string());
    }
    if !url
        .query_pairs()
        .any(|(key, value)| key == "sig" && !value.is_empty())
    {
        return Err("不是有效的 SAS URL（缺少 sig 參數）".to_string());
    }

    let segments: Vec<&str> = url
        .path_segments()
        .map(|parts| parts.filter(|part| !part.is_empty()).collect())
        .unwrap_or_default();
    if segments.is_empty() {
        return Err("SAS URL 必須包含容器名稱，例如 https://<account>.blob.core.windows.net/<container>?<SAS>".to_string());
    }

    let is_blob_sas = url
        .query_pairs()
        .any(|(key, value)| key == "sr" && value == "b");
    if is_blob_sas {
        if segments.len() < 2 {
            return Err("Blob SAS URL 必須包含 Blob 名稱".to_string());
        }
        return Ok(url);
    }

    url.path_segments_mut()
        .map_err(|_| "SAS URL 無法附加檔名".to_string())?
        .pop_if_empty()
        .push(file_name);
    Ok(url)
}

/// 供訊息顯示：移除 query string 與 fragment，避免洩漏 SAS 簽章。
pub(crate) fn display_url(url: &Url) -> String {
    let mut shown = url.clone();
    shown.set_query(None);
    shown.set_fragment(None);
    shown.to_string()
}

pub(crate) async fn upload_json(url: &Url, body: String) -> Result<(), String> {
    // 帶有憑證的 PUT 不跟隨重新導向。
    let client = reqwest::Client::builder()
        .timeout(UPLOAD_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("建立 HTTP 用戶端失敗: {}", e.without_url()))?;

    let response = client
        .put(url.clone())
        .header("x-ms-blob-type", "BlockBlob")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )
        .body(body)
        .send()
        .await
        .map_err(|e| format!("上傳失敗: {}", e.without_url()))?;

    let status = response.status();
    if status == StatusCode::CREATED {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    Err(format!(
        "Azure Blob 回應 HTTP {status}{}",
        error_detail(&body)
    ))
}

/// 從 Azure 的 XML 錯誤內容取出 `<Code>` 與 `<Message>` 首行；不回傳整段內容。
fn error_detail(body: &str) -> String {
    let code = xml_tag(body, "Code");
    let message = xml_tag(body, "Message")
        .and_then(|text| text.lines().next().map(str::trim).map(str::to_string));
    match (code, message) {
        (Some(code), Some(message)) => format!("（{code}: {message}）"),
        (Some(code), None) => format!("（{code}）"),
        _ => String::new(),
    }
}

fn xml_tag<'a>(body: &'a str, tag: &str) -> Option<&'a str> {
    let start = body.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = body[start..].find(&format!("</{tag}>"))? + start;
    Some(body[start..end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};

    const SAS: &str =
        "sv=2025-07-05&spr=https&st=2026-10-28T10%3A41%3A36Z&sr=c&sp=racwl&sig=abc%3D";

    #[test]
    fn url_detection_only_matches_http_schemes() {
        assert!(is_url("https://example.blob.core.windows.net/c?sig=x"));
        assert!(is_url("HTTP://example.com/c"));
        assert!(!is_url("all-usage.json"));
        assert!(!is_url(r"C:\exports\all.json"));
        assert!(!is_url("./https/all.json"));
    }

    #[test]
    fn file_name_uses_timestamp_and_sanitized_computer_name() {
        let now = FixedOffset::east_opt(8 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, 28, 9, 5, 7)
            .unwrap();
        assert_eq!(
            export_file_name(&now, "WILL-PC"),
            "2026-10-28_09-05-07_WILL-PC.json"
        );
        assert_eq!(
            export_file_name(&now, " My PC/01 "),
            "2026-10-28_09-05-07_My-PC-01.json"
        );
        assert_eq!(
            export_file_name(&now, "保哥的電腦"),
            "2026-10-28_09-05-07_保哥的電腦.json"
        );
        assert_eq!(
            export_file_name(&now, "  "),
            "2026-10-28_09-05-07_unknown-host.json"
        );
    }

    #[test]
    fn computer_name_is_never_empty_or_path_like() {
        let name = computer_name();
        assert!(!name.is_empty());
        assert!(!name.contains(['/', '\\', ' ']));
    }

    #[test]
    fn container_sas_gets_file_name_and_keeps_query_byte_identical() {
        for container_path in ["/container", "/container/", "/container/exports"] {
            let sas_url = format!("https://acct.blob.core.windows.net{container_path}?{SAS}");
            let url = build_blob_url(&sas_url, "2026-10-28_09-05-07_PC.json").unwrap();
            assert_eq!(url.query(), Some(SAS));
            let expected_path = format!(
                "{}/2026-10-28_09-05-07_PC.json",
                container_path.trim_end_matches('/')
            );
            assert_eq!(url.path(), expected_path);
        }
    }

    #[test]
    fn file_name_with_chinese_computer_name_is_percent_encoded_in_path() {
        let url = build_blob_url(
            &format!("https://acct.blob.core.windows.net/c?{SAS}"),
            "2026-10-28_09-05-07_保哥.json",
        )
        .unwrap();
        assert!(url.path().starts_with("/c/2026-10-28_09-05-07_%E4%BF%9D"));
    }

    #[test]
    fn blob_sas_is_used_as_is() {
        let sas_url = "https://acct.blob.core.windows.net/c/fixed.json?sr=b&sig=abc";
        let url = build_blob_url(sas_url, "ignored.json").unwrap();
        assert_eq!(url.as_str(), sas_url);
        assert!(build_blob_url(
            "https://acct.blob.core.windows.net/c?sr=b&sig=abc",
            "x.json"
        )
        .is_err());
    }

    #[test]
    fn invalid_sas_urls_are_rejected_without_echoing_the_signature() {
        let cases = [
            "http://acct.blob.core.windows.net/c?sig=secret",
            "https://acct.blob.core.windows.net/c?sv=2025-07-05",
            "https://acct.blob.core.windows.net/?sig=secret",
            "https://acct.blob.core.windows.net?sig=secret",
            "https://",
        ];
        for case in cases {
            let err = build_blob_url(case, "x.json").unwrap_err();
            assert!(!err.contains("secret"), "{err}");
        }
    }

    #[test]
    fn display_url_drops_the_query_string() {
        let url = build_blob_url(
            &format!("https://acct.blob.core.windows.net/c?{SAS}"),
            "x.json",
        )
        .unwrap();
        let shown = display_url(&url);
        assert_eq!(shown, "https://acct.blob.core.windows.net/c/x.json");
        assert!(!shown.contains("sig"));
    }

    #[test]
    fn error_detail_extracts_code_and_first_message_line() {
        let xml = "<?xml version=\"1.0\"?><Error><Code>AuthorizationFailure</Code><Message>This request is not authorized.\nRequestId:abc\nTime:now</Message></Error>";
        assert_eq!(
            error_detail(xml),
            "（AuthorizationFailure: This request is not authorized.）"
        );
        assert_eq!(error_detail("not xml"), "");
    }

    /// 以本機 TCP 伺服器回應固定內容，並回傳收到的完整請求。
    async fn serve_once(response: String) -> (Url, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                received.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&received);
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let length = text
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if received.len() >= head_end + 4 + length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&received).to_string()
        });
        let url = Url::parse(&format!("http://127.0.0.1:{port}/c/f.json?sig=secret")).unwrap();
        (url, handle)
    }

    #[tokio::test]
    async fn upload_sends_block_blob_put_and_accepts_201() {
        let (url, server) = serve_once(
            "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
        )
        .await;
        upload_json(&url, "{\"exports\":[]}".to_string())
            .await
            .unwrap();
        let request = server.await.unwrap();
        let lower = request.to_ascii_lowercase();
        assert!(request.starts_with("PUT /c/f.json?sig=secret HTTP/1.1"));
        assert!(lower.contains("x-ms-blob-type: blockblob"));
        assert!(lower.contains("content-type: application/json; charset=utf-8"));
        assert!(request.ends_with("{\"exports\":[]}"));
    }

    #[tokio::test]
    async fn upload_failure_reports_status_and_code_but_not_the_signature() {
        let body =
            "<Error><Code>AuthorizationPermissionMismatch</Code><Message>denied</Message></Error>";
        let response = format!(
            "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (url, server) = serve_once(response).await;
        let err = upload_json(&url, "{}".to_string()).await.unwrap_err();
        server.await.unwrap();
        assert!(err.contains("403"), "{err}");
        assert!(err.contains("AuthorizationPermissionMismatch"), "{err}");
        assert!(!err.contains("secret"), "{err}");
    }

    #[tokio::test]
    async fn upload_connection_error_does_not_leak_the_signature() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/c/f.json?sig=secret")).unwrap();
        let err = upload_json(&url, "{}".to_string()).await.unwrap_err();
        assert!(!err.contains("secret"), "{err}");
    }
}
