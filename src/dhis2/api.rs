//! The DHIS2 Web API, read-only: one signed-in client, `system/info` read
//! once at connect, and paged metadata lists.

use std::time::Duration;

use base64::Engine;
use serde_json::Value;

use crate::cli::Dhis2Conn;
use crate::error::{KilnError, Result};
use crate::extract::client::{Auth, FetchError, FhirClient};

const PAGE_SIZE: usize = 500;

/// What `system/info` says about the instance.
#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub version: Option<String>,
    pub system_name: Option<String>,
    /// When the analytics tables were last rebuilt; analytics values are as
    /// of this moment. DHIS2 local time, no zone.
    pub last_analytics: Option<String>,
    /// The server runs on UTC, so its timestamps can be given a `Z`.
    pub utc: bool,
}

impl ServerInfo {
    pub fn parse(v: &Value) -> Self {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let zone = s("serverTimeZoneId").unwrap_or_default();
        ServerInfo {
            version: s("version"),
            system_name: s("systemName"),
            last_analytics: s("lastAnalyticsTableSuccess"),
            utc: matches!(zone.as_str(), "UTC" | "Etc/UTC" | "GMT" | "Etc/GMT" | "Z"),
        }
    }

    /// A DHIS2 timestamp (`2026-10-12T02:07:00.052`, server local time) as a
    /// FHIR dateTime. FHIR needs a zone whenever there is a time, so on a
    /// server not known to run on UTC only the date is kept.
    pub fn timestamp(&self, ts: &str) -> Option<String> {
        let date_ok = ts.len() >= 10
            && ts.as_bytes()[4] == b'-'
            && ts.as_bytes()[7] == b'-'
            && ts[..4].bytes().all(|b| b.is_ascii_digit());
        if !date_ok {
            return None;
        }
        let has_time = ts.len() >= 19 && ts.as_bytes()[10] == b'T';
        if !has_time || !self.utc {
            return Some(ts[..10].to_string());
        }
        let ends_with_zone = ts.ends_with('Z') || ts[19..].contains('+') || ts[19..].contains('-');
        Some(if ends_with_zone {
            ts.to_string()
        } else {
            format!("{ts}Z")
        })
    }
}

/// `https://x/dhis/`, `https://x/dhis/api` → `https://x/dhis/api`.
pub fn api_base(url: &str) -> Result<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/api").unwrap_or(trimmed);
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(KilnError::Usage(format!(
            "--dhis2 must be an http(s) URL, got {url:?}"
        )));
    }
    Ok(format!("{trimmed}/api"))
}

pub fn auth(conn: &Dhis2Conn) -> Auth {
    match (&conn.token, &conn.user, &conn.password) {
        (Some(t), _, _) => Auth::Header(format!("ApiToken {t}")),
        (None, Some(u), Some(p)) => Auth::Header(format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))
        )),
        _ => Auth::None,
    }
}

pub struct Dhis2 {
    base: String,
    client: FhirClient,
    pub info: ServerInfo,
}

impl Dhis2 {
    /// Signs in by reading `system/info`, so bad credentials or a wrong URL
    /// fail here, before any other request.
    pub fn connect(conn: &Dhis2Conn) -> Result<Self> {
        let base = api_base(&conn.url)?;
        let client = FhirClient::new(auth(conn), conn.retries, Duration::from_secs(conn.timeout))?;
        let mut d = Dhis2 {
            base,
            client,
            info: ServerInfo::default(),
        };
        let info = d.get("system/info.json", &[])?;
        d.info = ServerInfo::parse(&info);
        Ok(d)
    }

    pub fn url(&self, path: &str, query: &[(&str, String)]) -> Result<String> {
        let mut url = url::Url::parse(&format!("{}/{path}", self.base))
            .map_err(|e| KilnError::Usage(format!("bad DHIS2 URL {}/{path}: {e}", self.base)))?;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }
        Ok(url.into())
    }

    /// GET and parse, keeping the HTTP error for callers that act on it
    /// (analytics splits a request DHIS2 calls too large).
    pub fn try_get(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<std::result::Result<Value, FetchError>> {
        let url = self.url(path, query)?;
        match self.client.get_json(&url) {
            Ok(f) => Ok(Ok(serde_json::from_slice(&f.body).map_err(|e| {
                KilnError::Environment(format!("{url}: response is not JSON: {e}"))
            })?)),
            Err(e) => Ok(Err(e)),
        }
    }

    pub fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        match self.try_get(path, query)? {
            Ok(v) => Ok(v),
            Err(e) => Err(self.error(path, e)),
        }
    }

    pub fn error(&self, path: &str, e: FetchError) -> KilnError {
        match e {
            FetchError::Status { status: 401, .. } | FetchError::Status { status: 403, .. } => {
                KilnError::Usage(format!(
                    "DHIS2 at {} refused the request ({e}); check --dhis2-token or --dhis2-user/--dhis2-password",
                    self.base
                ))
            }
            other => KilnError::Environment(format!("{}/{path}: {other}", self.base)),
        }
    }

    /// Every object under `key` across all pages of a metadata list.
    pub fn paged(&self, path: &str, key: &str, query: &[(&str, String)]) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut page = 1usize;
        loop {
            let mut q: Vec<(&str, String)> = query.to_vec();
            q.push(("page", page.to_string()));
            q.push(("pageSize", PAGE_SIZE.to_string()));
            let v = self.get(path, &q)?;
            match v.get(key).and_then(Value::as_array) {
                Some(items) => out.extend(items.iter().cloned()),
                None => {
                    return Err(KilnError::Environment(format!(
                        "{}/{path}: response has no {key:?} list",
                        self.base
                    )))
                }
            }
            let page_count = v
                .pointer("/pager/pageCount")
                .and_then(Value::as_u64)
                .unwrap_or(1) as usize;
            if page >= page_count {
                return Ok(out);
            }
            page += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_api_path_is_added_once() {
        assert_eq!(
            api_base("https://x.org/dhis").unwrap(),
            "https://x.org/dhis/api"
        );
        assert_eq!(
            api_base("https://x.org/dhis/").unwrap(),
            "https://x.org/dhis/api"
        );
        assert_eq!(
            api_base("https://x.org/dhis/api/").unwrap(),
            "https://x.org/dhis/api"
        );
        assert!(api_base("x.org").is_err());
    }

    #[test]
    fn timestamps_get_a_zone_only_on_a_utc_server() {
        let utc = ServerInfo::parse(&json!({"serverTimeZoneId": "Etc/UTC"}));
        assert_eq!(
            utc.timestamp("2026-10-12T02:07:00.052").as_deref(),
            Some("2026-10-12T02:07:00.052Z")
        );
        assert_eq!(utc.timestamp("2026-10-12").as_deref(), Some("2026-10-12"));
        assert_eq!(utc.timestamp("nonsense"), None);
        let local = ServerInfo::parse(&json!({"serverTimeZoneId": "Africa/Freetown"}));
        assert_eq!(
            local.timestamp("2026-10-12T02:07:00.052").as_deref(),
            Some("2026-10-12")
        );
    }

    #[test]
    fn credentials_become_one_authorization_value() {
        let conn = |token: Option<&str>, user: Option<&str>, password: Option<&str>| Dhis2Conn {
            url: "https://x".into(),
            instance: "x".into(),
            token: token.map(str::to_string),
            user: user.map(str::to_string),
            password: password.map(str::to_string),
            retries: 1,
            timeout: 1,
        };
        assert!(
            matches!(auth(&conn(Some("d2pat_1"), None, None)), Auth::Header(h) if h == "ApiToken d2pat_1")
        );
        assert!(
            matches!(auth(&conn(None, Some("admin"), Some("district"))), Auth::Header(h) if h == "Basic YWRtaW46ZGlzdHJpY3Q=")
        );
        assert!(matches!(auth(&conn(None, None, None)), Auth::None));
    }
}
