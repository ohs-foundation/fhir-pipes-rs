//! The retry policy: connection errors, 5xx and 429 are retried with
//! jittered exponential backoff, a Retry-After (or a 429's `RateLimit`
//! reset) replacing the computed delay; any other status is final.

use std::time::{Duration, Instant, SystemTime};

use reqwest::blocking::{RequestBuilder, Response};
use reqwest::header::{HeaderMap, RETRY_AFTER};

use super::body::read_error_body;
use super::{FetchError, FhirClient};

const RETRY_BASE: f64 = 0.5;

const RETRY_MAX: f64 = 30.0;

/// The longest wait read from a Retry-After. Far more than a server asks
/// for in practice; it keeps a huge value from overflowing a `Duration` or
/// an `Instant`.
const RETRY_AFTER_MAX: Duration = Duration::from_secs(24 * 60 * 60);

/// What `retrying` does before each attempt.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Attempt {
    /// Waits for a place in the gate, then adds the Authorization header as
    /// it is then, renewing a token that is due, when the request goes to
    /// the server's origin.
    Authorized,
    /// `Authorized`, to any origin: a Bulk Data output file whose manifest
    /// says it needs the token, which the spec lets live on another host.
    AuthorizedAnywhere,
    /// Waits for a place; no credentials (a signed storage URL).
    Anonymous,
    /// Neither: a token request. It runs while a request holds a place and
    /// the token lock, so it must not wait for another place.
    Token,
}

/// The wait before retry `attempt + 1`: the server's Retry-After, or 0.5 s
/// doubling with each attempt; either way at most RETRY_MAX.
pub fn backoff_delay(attempt: usize, retry_after: Option<Duration>) -> Duration {
    let max = Duration::from_secs_f64(RETRY_MAX);
    retry_after.map_or_else(
        || {
            let secs = RETRY_BASE * 2f64.powi(attempt.saturating_sub(1) as i32);
            Duration::from_secs_f64(secs.clamp(0.0, RETRY_MAX))
        },
        |wait| wait.min(max),
    )
}

/// A Retry-After value as a wait: delay-seconds or an HTTP-date (RFC 9110
/// section 10.2.3), clamped to [0, RETRY_AFTER_MAX]. Fractional seconds are
/// accepted, a negative number or a date in the past is no wait, and NaN or
/// anything else unreadable is None. The one parser for every caller, so no
/// server value can panic a `Duration` conversion.
pub fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let value = value?.trim();
    if let Ok(secs) = value.parse::<f64>() {
        if secs.is_nan() {
            return None;
        }
        let secs = secs.clamp(0.0, RETRY_AFTER_MAX.as_secs_f64());
        return Some(Duration::from_secs_f64(secs));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    let wait = at.duration_since(SystemTime::now()).unwrap_or_default();
    Some(wait.min(RETRY_AFTER_MAX))
}

/// The wait a response asks for before the next request: its Retry-After,
/// read by `parse_retry_after`, else, on a 429, the `t=` of its `RateLimit`
/// header, read by `parse_ratelimit_reset`. On any other status a
/// `RateLimit` header only reports the quota, so it is not a wait.
pub fn server_wait(status: u16, headers: &HeaderMap) -> Option<Duration> {
    let text = |name| headers.get(name).and_then(|v| v.to_str().ok());
    parse_retry_after(text(RETRY_AFTER)).or_else(|| {
        if status != 429 {
            return None;
        }
        // Several fields are one list (RFC 9110 section 5.3).
        let fields: Vec<&str> = headers
            .get_all("ratelimit")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        parse_ratelimit_reset((!fields.is_empty()).then(|| fields.join(", ")).as_deref())
    })
}

/// A `RateLimit` header's wait: the `t=` (seconds until the quota is
/// restored) of the policy that has run out, `r=0`, or the longest `t=`
/// when none says so. Medplum sends `"requests";r=0;t=60`. Clamped to
/// [0, RETRY_AFTER_MAX] as a Retry-After is; None when no item has a
/// readable `t=`.
pub fn parse_ratelimit_reset(value: Option<&str>) -> Option<Duration> {
    let mut spent = None;
    let mut any = None;
    for item in value?.split(',') {
        let mut remaining = None;
        let mut reset = None;
        for param in item.split(';').skip(1) {
            let Some((key, v)) = param.split_once('=') else {
                continue;
            };
            let v = v.trim().trim_matches('"');
            match key.trim() {
                "r" => remaining = v.parse::<f64>().ok(),
                "t" => reset = v.parse::<f64>().ok().filter(|t| !t.is_nan()),
                _ => {}
            }
        }
        let Some(t) = reset else {
            continue;
        };
        let wait = Duration::from_secs_f64(t.clamp(0.0, RETRY_AFTER_MAX.as_secs_f64()));
        any = any.max(Some(wait));
        if remaining.is_some_and(|r| r <= 0.0) {
            spent = spent.max(Some(wait));
        }
    }
    spent.or(any)
}

/// A factor in [0.5, 1.0) applied to a computed backoff delay. Eight
/// workers retrying in lockstep after a shared 429 is a self-inflicted
/// herd; spreading their sleeps out avoids re-synchronizing the retry.
/// A cheap thread-local xorshift is plenty here — this isn't cryptographic,
/// just enough spread to desynchronize concurrent retriers.
fn jitter_factor() -> f64 {
    thread_local! {
        static RNG: std::cell::Cell<u64> = std::cell::Cell::new(seed());
    }

    /// A different seed in every thread: `RandomState` keys come from the
    /// OS's random source, once per thread, and the thread id and the clock
    /// are hashed in as well.
    fn seed() -> u64 {
        use std::hash::BuildHasher as _;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        std::collections::hash_map::RandomState::new()
            .hash_one((std::thread::current().id(), nanos))
            | 1
    }

    fn xorshift(mut x: u64) -> u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    }

    RNG.with(|cell| {
        let next = xorshift(cell.get());
        cell.set(next);
        (next >> 11) as f64 / (1u64 << 53) as f64
    })
}

fn jittered(delay: Duration) -> Duration {
    let factor = 0.5 + 0.5 * jitter_factor();
    Duration::from_secs_f64(delay.as_secs_f64() * factor)
}

impl FhirClient {
    /// The retry loop with the Authorization header added to every attempt.
    pub(super) fn send<T>(
        &self,
        request: impl Fn() -> RequestBuilder,
        success: impl FnOnce(Response) -> std::result::Result<T, FetchError>,
    ) -> std::result::Result<T, FetchError> {
        self.retrying(Attempt::Authorized, request, success)
    }

    /// The retry loop. Any 2xx is a success, handed to `success`; 429 and
    /// 5xx retry with backoff (the wait the server asks for, by
    /// `server_wait`, replaces the computed delay); any other status is
    /// final on the first response.
    pub(super) fn retrying<T>(
        &self,
        kind: Attempt,
        request: impl Fn() -> RequestBuilder,
        success: impl FnOnce(Response) -> std::result::Result<T, FetchError>,
    ) -> std::result::Result<T, FetchError> {
        self.retrying_by(kind, request, |_| false, None, success)
    }

    /// `retrying`, with the statuses for which `handled` is true handed to
    /// `success` as a 2xx is, not retried. With a `deadline`, each attempt
    /// keeps the client's timeout, cut short when less time is left, the
    /// waits between attempts end by it, and no attempt starts after it;
    /// the last failure is then the error.
    pub(super) fn retrying_by<T>(
        &self,
        kind: Attempt,
        request: impl Fn() -> RequestBuilder,
        handled: impl Fn(u16) -> bool,
        deadline: Option<Instant>,
        success: impl FnOnce(Response) -> std::result::Result<T, FetchError>,
    ) -> std::result::Result<T, FetchError> {
        let left = || deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let sleep = |wait: Duration| std::thread::sleep(left().map_or(wait, |l| wait.min(l)));
        let mut last: Option<FetchError> = None;
        for attempt in 1..=self.retries {
            let pass = match kind {
                Attempt::Token => None,
                Attempt::Authorized | Attempt::AuthorizedAnywhere | Attempt::Anonymous => {
                    match self.enter_by(deadline) {
                        Ok(pass) => pass,
                        Err(()) => break,
                    }
                }
            };
            let mut request = request();
            // Chosen once admitted, for every attempt: the wait for a place
            // and the sleeps between attempts can outlast a token, and the
            // token is renewed here when it is due.
            let anywhere = kind == Attempt::AuthorizedAnywhere;
            if (kind == Attempt::Authorized || anywhere)
                && let Some(v) = self.authorization()?
            {
                request = self.authorize(request, &v, anywhere)?;
            }
            // After both: the wait for a place and a token renewal take from
            // the time left.
            if let Some(left) = left() {
                if left.is_zero() {
                    break;
                }
                request = request.timeout(self.timeout.min(left));
            }
            match request.send() {
                Err(e) => {
                    drop(pass);
                    last = Some(FetchError::Transport(e.to_string()));
                    if attempt < self.retries {
                        sleep(jittered(backoff_delay(attempt, None)));
                    }
                }
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if handled(status) {
                        return success(resp);
                    }
                    if status == 429 || status >= 500 {
                        let ra = server_wait(status, resp.headers());
                        let delay = backoff_delay(attempt, ra);
                        let delay = if ra.is_some() { delay } else { jittered(delay) };
                        last = Some(FetchError::Status {
                            status,
                            body: read_error_body(resp),
                        });
                        drop(pass);
                        if let (Some(gate), Some(_)) = (&self.gate, ra) {
                            // The server asked for a wait: every thread
                            // sharing the client waits, not only this one.
                            gate.pause(delay);
                        }
                        if attempt < self.retries {
                            sleep(delay);
                        }
                    } else if !(200..300).contains(&status) {
                        return Err(FetchError::Status {
                            status,
                            body: read_error_body(resp),
                        });
                    } else {
                        return success(resp);
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            let why = if deadline.is_some() {
                "the deadline passed"
            } else {
                "no attempts made"
            };
            FetchError::Transport(why.to_string())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    use crate::http::tests::client;
    use httptest::{Expectation, Server, matchers::*, responders::*};

    #[test]
    fn backoff_doubles_from_half_a_second_and_is_clamped() {
        assert_eq!(backoff_delay(1, None).as_millis(), 500);
        assert_eq!(backoff_delay(2, None).as_millis(), 1000);
        assert_eq!(backoff_delay(3, None).as_millis(), 2000);
        assert_eq!(backoff_delay(20, None).as_secs(), 30);
        assert_eq!(backoff_delay(1, Some(Duration::from_secs(7))).as_secs(), 7);
        assert_eq!(backoff_delay(1, Some(RETRY_AFTER_MAX)).as_secs(), 30);
        assert_eq!(backoff_delay(1, Some(Duration::ZERO)).as_secs(), 0);
    }

    #[test]
    fn retry_after_reads_seconds_and_http_dates_and_clamps() {
        let secs = |v: &str| parse_retry_after(Some(v));
        assert_eq!(secs("7"), Some(Duration::from_secs(7)));
        assert_eq!(secs(" 1.5 "), Some(Duration::from_millis(1500)));
        // Out of range: never a panic, clamped to [0, RETRY_AFTER_MAX].
        assert_eq!(secs("1e300"), Some(RETRY_AFTER_MAX));
        assert_eq!(secs("inf"), Some(RETRY_AFTER_MAX));
        assert_eq!(secs("-5"), Some(Duration::ZERO));
        assert_eq!(secs("-1e300"), Some(Duration::ZERO));
        assert_eq!(secs("-inf"), Some(Duration::ZERO));
        assert_eq!(secs("NaN"), None);
        assert_eq!(secs("soon"), None);
        assert_eq!(secs(""), None);
        assert_eq!(parse_retry_after(None), None);
        // An HTTP-date: the time until then, nothing for one in the past,
        // RETRY_AFTER_MAX for one far ahead.
        assert_eq!(secs("Sun, 06 Nov 1994 08:49:37 GMT"), Some(Duration::ZERO));
        assert_eq!(secs("Fri, 31 Dec 9999 23:59:59 GMT"), Some(RETRY_AFTER_MAX));
        let soon = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(120));
        let wait = secs(&soon).unwrap();
        assert!(
            wait > Duration::from_secs(110) && wait <= Duration::from_secs(120),
            "{wait:?}"
        );
    }

    #[test]
    fn a_ratelimit_header_gives_the_reset_of_the_spent_policy() {
        let reset = |v: &str| parse_ratelimit_reset(Some(v));
        assert_eq!(
            reset(r#""requests";r=0;t=60"#),
            Some(Duration::from_secs(60))
        );
        assert_eq!(reset("default;r=0;t=2"), Some(Duration::from_secs(2)));
        // The policy that has run out, not the longer wait of one that has not.
        assert_eq!(
            reset(r#""fhir";r=500;t=50, "requests";r=0;t=30"#),
            Some(Duration::from_secs(30))
        );
        // None has run out: the longest.
        assert_eq!(
            reset(r#""a";r=5;t=10, "b";r=3;t=40"#),
            Some(Duration::from_secs(40))
        );
        assert_eq!(reset(r#""a";r=0;t=1e300"#), Some(RETRY_AFTER_MAX));
        assert_eq!(reset(r#""a";r=0;t=-3"#), Some(Duration::ZERO));
        assert_eq!(reset(r#""a";r=0;t=NaN"#), None);
        // The older draft's form has no `t=`.
        assert_eq!(reset("limit=100, remaining=0, reset=60"), None);
        assert_eq!(reset(""), None);
        assert_eq!(parse_ratelimit_reset(None), None);
    }

    #[test]
    fn a_ratelimit_header_is_a_wait_only_on_a_429_without_a_retry_after() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.append(*k, HeaderValue::from_static(v));
            }
            h
        };
        let limited = headers(&[("ratelimit", "\"requests\";r=0;t=7")]);
        assert_eq!(server_wait(429, &limited), Some(Duration::from_secs(7)));
        assert_eq!(server_wait(202, &limited), None);
        assert_eq!(server_wait(503, &limited), None);
        let both = headers(&[("retry-after", "3"), ("ratelimit", "\"requests\";r=0;t=7")]);
        assert_eq!(server_wait(429, &both), Some(Duration::from_secs(3)));
        // Two RateLimit fields are read as one list.
        let two = headers(&[
            ("ratelimit", "\"fhir\";r=9;t=50"),
            ("ratelimit", "\"requests\";r=0;t=20"),
        ]);
        assert_eq!(server_wait(429, &two), Some(Duration::from_secs(20)));
    }

    #[test]
    fn medplums_two_ratelimit_fields_are_read_as_it_sends_them() {
        // Self-hosted Medplum 5.2.5 sends both fields on every response and
        // no Retry-After on a 429.
        let headers = |requests: &'static str, fhir: &'static str| {
            let mut h = HeaderMap::new();
            h.append("ratelimit", HeaderValue::from_static(requests));
            h.append("ratelimit", HeaderValue::from_static(fhir));
            h
        };
        let quota = headers(
            "\"requests\";r=59990;t=60",
            "\"fhirInteractions\";r=49980;t=60",
        );
        // Not a 429: the fields only report the quota.
        assert_eq!(server_wait(200, &quota), None);
        assert_eq!(server_wait(202, &quota), None);
        // A 429 with neither spent: the longest reset.
        assert_eq!(server_wait(429, &quota), Some(Duration::from_secs(60)));
        // One spent: its reset, not the other's.
        let spent = headers("\"requests\";r=59990;t=60", "\"fhirInteractions\";r=0;t=45");
        assert_eq!(server_wait(429, &spent), Some(Duration::from_secs(45)));
        let spent = headers("\"requests\";r=0;t=12", "\"fhirInteractions\";r=49980;t=60");
        assert_eq!(server_wait(429, &spent), Some(Duration::from_secs(12)));
    }

    #[test]
    fn each_thread_draws_its_own_jitter() {
        // Seeded from the clock's elapsed nanoseconds, every thread's first
        // factor was about 0, so retries after a shared 5xx stayed in step.
        let threads: Vec<_> = (0..8).map(|_| std::thread::spawn(jitter_factor)).collect();
        let mut firsts: Vec<f64> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(firsts.iter().all(|f| (0.0..1.0).contains(f)), "{firsts:?}");
        firsts.sort_by(f64::total_cmp);
        assert!(firsts[7] - firsts[0] > 0.01, "{firsts:?}");
        firsts.dedup();
        assert_eq!(firsts.len(), 8, "{firsts:?}");
    }

    #[test]
    fn a_500_then_200_succeeds_with_retries() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/b"))
                .times(2)
                .respond_with(cycle![status_code(500), status_code(200).body("ok")]),
        );
        let got = client(3).get(&server.url("/b").to_string()).unwrap();
        assert_eq!(got.body, b"ok");
    }

    #[test]
    fn a_429_honours_retry_after() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/r"))
                .times(2)
                .respond_with(cycle![
                    status_code(429).append_header("Retry-After", "1"),
                    status_code(200).body("ok")
                ]),
        );
        let t = std::time::Instant::now();
        client(3).get(&server.url("/r").to_string()).unwrap();
        assert!(t.elapsed() >= std::time::Duration::from_secs(1));
    }

    #[test]
    fn a_429_with_only_a_ratelimit_header_waits_for_its_reset() {
        // Medplum's form: no Retry-After. The backoff alone would retry
        // after half a second.
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/r"))
                .times(2)
                .respond_with(cycle![
                    status_code(429).append_header("RateLimit", "\"requests\";r=0;t=2"),
                    status_code(200).body("ok")
                ]),
        );
        let t = Instant::now();
        client(3).get(&server.url_str("/r")).unwrap();
        assert!(t.elapsed() >= Duration::from_secs(2), "{:?}", t.elapsed());
    }

    #[test]
    fn a_429_with_medplums_two_ratelimit_fields_waits_for_the_spent_one() {
        // The `requests` policy's 60 s would be capped at 30 s; the spent
        // `fhirInteractions` policy asks for 2 s.
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/r"))
                .times(2)
                .respond_with(cycle![
                    status_code(429)
                        .append_header("RateLimit", "\"requests\";r=59990;t=60")
                        .append_header("RateLimit", "\"fhirInteractions\";r=0;t=2"),
                    status_code(200).body("ok")
                ]),
        );
        let t = Instant::now();
        client(3).get(&server.url_str("/r")).unwrap();
        assert!(t.elapsed() >= Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    }

    #[test]
    fn a_bulk_kick_off_hands_a_429_back_without_retrying() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/fhir/$export"))
                .times(1)
                .respond_with(status_code(429).append_header("Retry-After", "5")),
        );
        let got = client(3)
            .get_async(&server.url_str("/fhir/$export"), None)
            .unwrap();
        assert_eq!(got.status, 429);
        assert_eq!(got.retry_after(), Some(Duration::from_secs(5)));
    }

    #[test]
    fn a_404_is_not_retried() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/m"))
                .times(1)
                .respond_with(status_code(404)),
        );
        let err = client(3).get(&server.url("/m").to_string()).unwrap_err();
        assert!(
            matches!(err, FetchError::Status { status: 404, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn exhausted_retries_report_the_last_status() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/x"))
                .times(2)
                .respond_with(status_code(503)),
        );
        let err = client(2).get(&server.url("/x").to_string()).unwrap_err();
        assert!(matches!(err, FetchError::Status { status: 503, .. }));
    }

    #[test]
    fn connection_refused_is_a_transport_error() {
        let err = client(1).get("http://127.0.0.1:9/nothing").unwrap_err();
        assert!(matches!(err, FetchError::Transport(_)), "{err:?}");
    }

    #[test]
    fn retry_after_is_honoured_on_a_503() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/s"))
                .times(2)
                .respond_with(cycle![
                    status_code(503).append_header("Retry-After", "1"),
                    status_code(200).body("ok")
                ]),
        );
        let t = std::time::Instant::now();
        client(3).get(&server.url("/s").to_string()).unwrap();
        assert!(t.elapsed() >= std::time::Duration::from_secs(1));
    }

    #[test]
    fn an_http_date_retry_after_is_honoured() {
        // Two seconds ahead, to whole seconds: a wait of more than one.
        let at = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(2));
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/d"))
                .times(2)
                .respond_with(cycle![
                    status_code(503).append_header("Retry-After", at.as_str()),
                    status_code(200).body("ok")
                ]),
        );
        let t = std::time::Instant::now();
        client(3).get(&server.url("/d").to_string()).unwrap();
        let elapsed = t.elapsed();
        assert!(elapsed >= Duration::from_millis(900), "{elapsed:?}");
        assert!(elapsed <= Duration::from_secs(5), "{elapsed:?}");
    }

    #[test]
    fn a_huge_retry_after_is_clamped_to_the_backoff_ceiling() {
        // One attempt, so the clamped wait is not slept; the point is that
        // the value does not panic the conversion or the gate's pause.
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/h"))
                .times(1)
                .respond_with(status_code(503).append_header("Retry-After", "1e300")),
        );
        let c = client(1).with_concurrency(2);
        let err = c.get(&server.url_str("/h")).unwrap_err();
        assert!(
            matches!(err, FetchError::Status { status: 503, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn errors_reflect_the_final_attempt() {
        // Both attempts fail to connect: the reported error is the transport error.
        let err = client(2).get("http://127.0.0.1:9/nothing").unwrap_err();
        assert!(matches!(err, FetchError::Transport(_)), "{err:?}");

        // Both attempts get a 503: the reported error is that status.
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/y"))
                .times(2)
                .respond_with(status_code(503)),
        );
        let err = client(2).get(&server.url("/y").to_string()).unwrap_err();
        assert!(
            matches!(err, FetchError::Status { status: 503, .. }),
            "{err:?}"
        );
    }
}
