//! The `[serve]` section of fhir-pipes.toml. `refresh` reads the same file
//! and ignores this section.

use std::time::Duration;

use anyhow::{Result, bail};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServeFile {
    retries: Option<u32>,
    retry_delay: Option<u64>,
    stop_grace: Option<u64>,
    keep_runs: Option<usize>,
    max_log_bytes: Option<u64>,
    #[serde(default)]
    schedule: Vec<ScheduleFile>,
    #[serde(default)]
    notify: Vec<NotifyFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleFile {
    cron: String,
    #[serde(default)]
    mode: Mode,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyFile {
    name: String,
    url: String,
    header: Option<String>,
    value_env: Option<String>,
}

/// An incremental or a full refresh.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Incremental,
    Full,
}

#[derive(Debug, Clone)]
pub struct Schedule {
    pub cron: croner::Cron,
    pub mode: Mode,
}

/// A notify destination: its name is what records show.
#[derive(Debug, Clone)]
pub struct Destination {
    pub name: String,
    pub url: url::Url,
    /// (header name, environment variable holding its value).
    pub header: Option<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub retries: u32,
    pub retry_delay: Duration,
    pub stop_grace: Duration,
    pub keep_runs: usize,
    pub max_log_bytes: u64,
    pub schedule: Vec<Schedule>,
    pub notify: Vec<Destination>,
}

/// The `[serve]` section, or the defaults without one.
pub fn parse(section: Option<toml::Table>) -> Result<ServeConfig> {
    let file: ServeFile = match section {
        Some(t) => toml::Value::Table(t).try_into()?,
        None => ServeFile::default(),
    };
    let mut schedule = Vec::new();
    for s in file.schedule {
        let cron = croner::Cron::new(&s.cron)
            .parse()
            .map_err(|e| anyhow::anyhow!("[[serve.schedule]] cron = \"{}\": {e}", s.cron))?;
        schedule.push(Schedule { cron, mode: s.mode });
    }
    let mut notify: Vec<Destination> = Vec::new();
    for n in file.notify {
        let name = n.name;
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("[[serve.notify]] name = \"{name}\": use letters, digits, - and _");
        }
        if notify.iter().any(|d| d.name == name) {
            bail!("[[serve.notify]] name = \"{name}\" is used twice");
        }
        // The URL is not quoted in these messages: it can carry a key.
        let Ok(url) = url::Url::parse(&n.url) else {
            bail!("notify {name}: the url is not a URL");
        };
        if !matches!(url.scheme(), "http" | "https") {
            bail!("notify {name}: the url must be http or https");
        }
        if !url.username().is_empty() || url.password().is_some() {
            bail!(
                "notify {name}: the url must not have a user name or password; use header and value_env"
            );
        }
        let header = match (n.header, n.value_env) {
            (Some(h), Some(v)) => Some((h, v)),
            (None, None) => None,
            _ => bail!("notify {name}: header and value_env go together"),
        };
        notify.push(Destination { name, url, header });
    }
    Ok(ServeConfig {
        retries: file.retries.unwrap_or(2),
        retry_delay: Duration::from_secs(file.retry_delay.unwrap_or(600)),
        stop_grace: Duration::from_secs(file.stop_grace.unwrap_or(120)),
        keep_runs: file.keep_runs.unwrap_or(100),
        max_log_bytes: file.max_log_bytes.unwrap_or(20_000_000),
        schedule,
        notify,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(text: &str) -> Option<toml::Table> {
        Some(toml::from_str(text).unwrap())
    }

    #[test]
    fn the_defaults_are_the_specs() {
        let c = parse(None).unwrap();
        assert_eq!(c.retries, 2);
        assert_eq!(c.retry_delay, Duration::from_secs(600));
        assert_eq!(c.stop_grace, Duration::from_secs(120));
        assert_eq!(c.keep_runs, 100);
        assert_eq!(c.max_log_bytes, 20_000_000);
    }

    #[test]
    fn an_unknown_key_is_refused() {
        assert!(parse(section("retrys = 1")).is_err());
    }

    #[test]
    fn a_header_needs_its_variable() {
        let err = parse(section(
            "[[notify]]\nname = \"a\"\nurl = \"https://x.example/i\"\nheader = \"x-api-key\"",
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("header and value_env go together"), "{err}");
    }
}
