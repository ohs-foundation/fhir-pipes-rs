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

/// Seconds since 1970 of a DHIS2 timestamp (`2026-10-12T02:07:00.052`, or
/// a date alone), read as UTC: on a server in another time zone it is off
/// by those hours, which does not matter for an age in days.
pub fn epoch_seconds(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let s = ts.get(r)?;
        s.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| s.parse().ok())?
    };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let (hh, mm, ss) = if b.len() >= 19 && b[10] == b'T' {
        (num(11..13)?, num(14..16)?, num(17..19)?)
    } else {
        (0, 0, 0)
    };
    // Days from 1970-01-01 to y-m-d in the proleptic Gregorian calendar
    // (Howard Hinnant's days_from_civil).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// `--max-analytics-age`: `7d`, `36h`, `2w`.
pub fn parse_age(s: &str) -> std::result::Result<Duration, String> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let secs = match unit {
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => {
            return Err(format!(
                "{s:?}: expected a number of hours, days or weeks, e.g. 36h, 7d, 2w"
            ))
        }
    };
    match n.parse::<u64>() {
        Ok(n) if n > 0 => Ok(Duration::from_secs(n * secs)),
        _ => Err(format!(
            "{s:?}: expected a whole number before {unit}, e.g. 7{unit}"
        )),
    }
}

/// `28 days`, `36 hours`.
pub fn age_text(age: Duration) -> String {
    let h = age.as_secs() / 3600;
    if h >= 48 {
        format!("{} days", h / 24)
    } else {
        format!("{h} hours")
    }
}

/// Values are only as fresh as DHIS2's last analytics build. Over `max`,
/// the run is refused; with no `max`, a build over a week old is only
/// warned about (the returned line). `now` is seconds since 1970.
pub fn check_analytics_age(
    info: &ServerInfo,
    max: Option<Duration>,
    now: i64,
) -> Result<Option<String>> {
    const WARN_AFTER: Duration = Duration::from_secs(7 * 86_400);
    let built = info.last_analytics.as_deref();
    let Some(at) = built.and_then(epoch_seconds) else {
        return match max {
            Some(_) => Err(KilnError::Usage(
                "--max-analytics-age: DHIS2 does not say when its analytics tables were last built, so their age cannot be checked".into(),
            )),
            None => Ok(None),
        };
    };
    let built = built.unwrap_or_default();
    let age = Duration::from_secs((now - at).max(0) as u64);
    match max {
        Some(max) if age > max => Err(KilnError::Usage(format!(
            "DHIS2 analytics tables were last built {built}, {} ago, older than --max-analytics-age {}; run analytics in DHIS2 (Data Administration → Analytics Tables) or allow an older build",
            age_text(age),
            age_text(max)
        ))),
        None if age > WARN_AFTER => Ok(Some(format!(
            "warning: DHIS2 analytics tables were last built {} ago; values are as of {built} (--max-analytics-age refuses an old build)",
            age_text(age)
        ))),
        _ => Ok(None),
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
    fn timestamps_count_seconds_from_1970() {
        assert_eq!(epoch_seconds("1970-01-01"), Some(0));
        assert_eq!(epoch_seconds("2000-03-01T00:00:00"), Some(951_868_800));
        assert_eq!(
            epoch_seconds("2026-10-12T02:07:00.052"),
            Some(1_791_770_820)
        );
        assert_eq!(epoch_seconds("nonsense"), None);
        assert_eq!(epoch_seconds("2026-13-01"), None);
    }

    #[test]
    fn ages_are_hours_days_or_weeks() {
        assert_eq!(parse_age("36h"), Ok(Duration::from_secs(36 * 3600)));
        assert_eq!(parse_age("7d"), Ok(Duration::from_secs(7 * 86_400)));
        assert_eq!(parse_age("2w"), Ok(Duration::from_secs(14 * 86_400)));
        for bad in ["7", "d", "0d", "7m", "-1d", "1.5d"] {
            assert!(parse_age(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_old_analytics_build_is_warned_about_or_refused() {
        let info = ServerInfo {
            last_analytics: Some("2026-09-15T03:00:00.000".into()),
            ..Default::default()
        };
        let now = epoch_seconds("2026-10-13T12:00:00").unwrap();
        let warning = check_analytics_age(&info, None, now).unwrap().unwrap();
        assert!(warning.contains("28 days ago"), "{warning}");
        let e = check_analytics_age(&info, parse_age("7d").ok(), now).unwrap_err();
        assert!(
            e.to_string().contains("last built 2026-09-15T03:00:00.000, 28 days ago, older than --max-analytics-age 7 days"),
            "{e}"
        );
        assert!(check_analytics_age(&info, parse_age("5w").ok(), now)
            .unwrap()
            .is_none());
        let fresh = epoch_seconds("2026-09-16T03:00:00").unwrap();
        assert!(check_analytics_age(&info, None, fresh).unwrap().is_none());
        assert!(check_analytics_age(&ServerInfo::default(), None, now)
            .unwrap()
            .is_none());
        assert!(check_analytics_age(&ServerInfo::default(), parse_age("7d").ok(), now).is_err());
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
