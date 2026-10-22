//! Telling the configured destinations that a run has ended. Delivery is at
//! least once: the body carries an `event_id` a receiver can use to ignore
//! a repeat. Each destination's state is kept in the run's record, so a
//! restart sends only to those still pending. One thread sends, one
//! destination at a time, apart from the runs and the API.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

use super::config::Destination;
use super::secrets::Redactor;
use super::store::{DeliveryState, RunId, RunRecord, Store};

/// How long the sender waits, between tries and on the wire.
#[derive(Clone, Copy, Debug)]
pub struct Times {
    /// Before the second and the third try.
    pub delays: [Duration; 2],
    pub connect: Duration,
    pub overall: Duration,
}

impl Default for Times {
    fn default() -> Times {
        Times {
            delays: [Duration::from_secs(10), Duration::from_secs(60)],
            connect: Duration::from_secs(10),
            overall: Duration::from_secs(30),
        }
    }
}

/// At most this much of a destination's answer is read.
const MAX_RESPONSE: u64 = 64 * 1024;

/// The body sent for a run that has ended.
pub fn body(record: &RunRecord, r: &Redactor) -> Value {
    let id = record.id.as_str();
    let mut b = json!({
        "event_id": format!("{id}:finished"),
        "run_id": id,
        "trigger": record.trigger,
        "status": record.status,
        "attempts": record.attempts.len(),
        "result": record.attempts.last().and_then(|a| a.result.clone()),
        "run_url": format!("/runs/{id}"),
    });
    r.redact_json(&mut b);
    b
}

pub struct Notifier {
    tx: mpsc::Sender<RunId>,
}

impl Notifier {
    /// The sender. Runs left with a destination pending (a restart) are sent
    /// first; destinations no longer in the config are dropped from them.
    pub fn start(
        store: Arc<Store>,
        destinations: Vec<Destination>,
        redactor: Arc<Redactor>,
        times: Times,
        out: Arc<super::output::StdoutQueue>,
        metrics: Option<Arc<super::metrics::Metrics>>,
    ) -> Notifier {
        let (tx, rx) = mpsc::channel::<RunId>();
        let left = match store.pending() {
            Ok(runs) => runs,
            Err(e) => {
                eprintln!("fhir-pipes: warning: {}", crate::error::message(&e));
                Vec::new()
            }
        };
        for r in &left {
            let _ = tx.send(r.id.clone());
        }
        std::thread::spawn(move || {
            crate::http::ensure_crypto_provider();
            let client = match reqwest::blocking::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(times.connect)
                .timeout(times.overall)
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("fhir-pipes: error: no HTTP client for notify: {e}");
                    return;
                }
            };
            let sender = Sender {
                store,
                client,
                destinations,
                redactor,
                times,
                out,
                metrics,
            };
            for id in rx {
                sender.deliver(&id);
            }
        });
        Notifier { tx }
    }

    /// Send run `id`'s pending destinations.
    pub fn enqueue(&self, id: RunId) {
        let _ = self.tx.send(id);
    }
}

/// What the sending thread works with.
struct Sender {
    store: Arc<Store>,
    client: reqwest::blocking::Client,
    destinations: Vec<Destination>,
    redactor: Arc<Redactor>,
    times: Times,
    out: Arc<super::output::StdoutQueue>,
    metrics: Option<Arc<super::metrics::Metrics>>,
}

impl Sender {
    /// Send run `id` to each of its pending destinations, saving its state
    /// after every try.
    fn deliver(&self, id: &RunId) {
        let store = &*self.store;
        let Ok(mut rec) = store.load(id) else {
            return;
        };
        // A destination taken out of the config is not sent old runs.
        let before = rec.notify.len();
        rec.notify
            .retain(|name, _| self.destinations.iter().any(|d| &d.name == name));
        if rec.notify.len() != before {
            save(store, &rec);
        }
        let text = body(&rec, &self.redactor).to_string();
        for d in &self.destinations {
            if rec.notify.get(&d.name).map(|x| x.state) != Some(DeliveryState::Pending) {
                continue;
            }
            loop {
                let tried = post(&self.client, d, &text).map_err(|e| self.redactor.redact(&e));
                let delivery = rec.notify.get_mut(&d.name).expect("checked above");
                delivery.tries += 1;
                match tried {
                    Ok(()) => {
                        delivery.state = DeliveryState::Delivered;
                        delivery.last_error = None;
                        self.out
                            .push(format!("{} notify {} delivered", id.as_str(), d.name));
                    }
                    Err(e) => {
                        self.out
                            .push(format!("{} notify {} failed: {e}", id.as_str(), d.name));
                        if let Some(m) = &self.metrics {
                            m.notify_failed(&d.name);
                        }
                        delivery.last_error = Some(e);
                        if delivery.tries >= 3 {
                            delivery.state = DeliveryState::Failed;
                        }
                    }
                }
                let tries = delivery.tries;
                let state = delivery.state;
                save(store, &rec);
                if state != DeliveryState::Pending {
                    break;
                }
                std::thread::sleep(self.times.delays[(tries as usize - 1).min(1)]);
            }
        }
    }
}

fn post(client: &reqwest::blocking::Client, d: &Destination, body: &str) -> Result<(), String> {
    let mut req = client
        .post(d.url.clone())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string());
    if let Some((name, env)) = &d.header {
        let value = std::env::var(env)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("{env} is not set"))?;
        req = req.header(name.as_str(), value);
    }
    let resp = req.send().map_err(|e| {
        if e.is_timeout() {
            "no answer in time".to_string()
        } else {
            // Without the URL, which can carry a key.
            format!("{}", e.without_url())
        }
    })?;
    let status = resp.status();
    // The answer is read, up to a limit, so the connection can be reused.
    let _ = std::io::copy(
        &mut std::io::Read::take(resp, MAX_RESPONSE),
        &mut std::io::sink(),
    );
    if status.is_success() {
        Ok(())
    } else if status.is_redirection() {
        Err(format!("{status}: redirects are not followed"))
    } else {
        Err(status.to_string())
    }
}

fn save(store: &Store, rec: &RunRecord) {
    if let Err(e) = store.save(rec) {
        eprintln!("fhir-pipes: warning: {}", crate::error::message(&e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::store::{Attempt, Delivery, DeliveryState, RunRequest, RunStatus, Trigger};
    use httptest::{Expectation, Server, matchers::*, responders::*};
    use std::collections::BTreeMap;
    use std::time::Instant;

    fn destination(name: &str, url: String) -> Destination {
        Destination {
            name: name.into(),
            url: url::Url::parse(&url).unwrap(),
            header: None,
        }
    }

    fn ended(store: &Store, names: &[(&str, DeliveryState)]) -> RunRecord {
        let mut r = RunRecord::new(
            RunId::new(chrono::Utc::now()),
            Trigger::Api,
            RunRequest::default(),
            "t".into(),
        );
        r.status = RunStatus::Failed;
        r.attempts.push(Attempt {
            started_at: "a".into(),
            finished_at: Some("b".into()),
            exit_code: Some(1),
            result: Some(json!({"status": "failed", "failed_step": "views", "error": "down"})),
        });
        r.notify = names
            .iter()
            .map(|(n, s)| {
                (
                    n.to_string(),
                    Delivery {
                        state: *s,
                        tries: 0,
                        last_error: None,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        store.create(&mut r).unwrap();
        r
    }

    fn sink() -> Arc<crate::serve::output::StdoutQueue> {
        crate::serve::output::StdoutQueue::start(Box::new(std::io::sink()))
    }

    fn quick() -> Times {
        Times {
            delays: [Duration::ZERO, Duration::ZERO],
            connect: Duration::from_secs(1),
            overall: Duration::from_secs(1),
        }
    }

    /// Wait until no destination of run `id` is pending.
    fn delivered(store: &Store, id: &RunId) -> RunRecord {
        let started = Instant::now();
        loop {
            let r = store.load(id).unwrap();
            if !r.pending() {
                return r;
            }
            assert!(started.elapsed() < Duration::from_secs(15), "{r:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_body_matches_the_spec() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("runs")).unwrap();
        let r = ended(&store, &[]);
        let b = body(&r, &Redactor::default());
        let id = r.id.as_str();
        assert_eq!(b["event_id"], format!("{id}:finished"));
        assert_eq!(b["run_id"], id);
        assert_eq!(b["trigger"], "api");
        assert_eq!(b["status"], "failed");
        assert_eq!(b["attempts"], 1);
        assert_eq!(b["result"]["failed_step"], "views");
        assert_eq!(b["run_url"], format!("/runs/{id}"));
    }

    #[test]
    fn a_lost_response_is_sent_again_with_the_same_event_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("runs")).unwrap());
        let r = ended(&store, &[("a", DeliveryState::Pending)]);
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("POST", "/i"),
                request::body(json_decoded(eq(body(&r, &Redactor::default())))),
            ])
            .times(2)
            .respond_with(cycle![
                delay_and_then(Duration::from_secs(3), status_code(200)),
                status_code(200),
            ]),
        );
        let n = Notifier::start(
            store.clone(),
            vec![destination("a", server.url_str("/i"))],
            Arc::default(),
            quick(),
            sink(),
            None,
        );
        n.enqueue(r.id.clone());
        let done = delivered(&store, &r.id);
        assert_eq!(done.notify["a"].state, DeliveryState::Delivered);
        assert_eq!(done.notify["a"].tries, 2);
    }

    #[test]
    fn a_restart_resends_only_to_pending() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("runs")).unwrap());
        let r = ended(
            &store,
            &[
                ("a", DeliveryState::Delivered),
                ("b", DeliveryState::Pending),
            ],
        );
        let a = Server::run();
        a.expect(
            Expectation::matching(any())
                .times(0)
                .respond_with(status_code(200)),
        );
        let b = Server::run();
        b.expect(
            Expectation::matching(request::method_path("POST", "/b"))
                .times(1)
                .respond_with(status_code(204)),
        );
        let _n = Notifier::start(
            store.clone(),
            vec![
                destination("a", a.url_str("/a")),
                destination("b", b.url_str("/b")),
            ],
            Arc::default(),
            quick(),
            sink(),
            None,
        );
        let done = delivered(&store, &r.id);
        assert_eq!(done.notify["b"].state, DeliveryState::Delivered);
    }

    #[test]
    fn a_destination_gone_from_the_config_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("runs")).unwrap());
        let r = ended(&store, &[("gone", DeliveryState::Pending)]);
        let _n = Notifier::start(store.clone(), vec![], Arc::default(), quick(), sink(), None);
        let done = delivered(&store, &r.id);
        assert!(done.notify.is_empty(), "{:?}", done.notify);
    }

    #[test]
    fn a_redirect_is_a_failed_try() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("runs")).unwrap());
        let r = ended(&store, &[("a", DeliveryState::Pending)]);
        let other = Server::run();
        other.expect(
            Expectation::matching(any())
                .times(0)
                .respond_with(status_code(200)),
        );
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("POST", "/i"))
                .times(3)
                .respond_with(
                    status_code(307).insert_header("Location", other.url_str("/x").as_str()),
                ),
        );
        let n = Notifier::start(
            store.clone(),
            vec![destination("a", server.url_str("/i"))],
            Arc::default(),
            quick(),
            sink(),
            None,
        );
        n.enqueue(r.id.clone());
        let done = delivered(&store, &r.id);
        assert_eq!(done.notify["a"].state, DeliveryState::Failed);
        assert_eq!(done.notify["a"].tries, 3);
        assert!(
            done.notify["a"]
                .last_error
                .as_deref()
                .unwrap()
                .contains("307"),
            "{:?}",
            done.notify["a"]
        );
        // A failed notify leaves the run's own status.
        assert_eq!(done.status, RunStatus::Failed);
    }

    #[test]
    fn a_server_that_never_answers_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("runs")).unwrap());
        let r = ended(&store, &[("a", DeliveryState::Pending)]);
        let server = Server::run();
        server.expect(
            Expectation::matching(any())
                .times(3)
                .respond_with(delay_and_then(Duration::from_secs(5), status_code(200))),
        );
        let started = Instant::now();
        let n = Notifier::start(
            store.clone(),
            vec![destination("a", server.url_str("/i"))],
            Arc::default(),
            quick(),
            sink(),
            None,
        );
        n.enqueue(r.id.clone());
        let done = delivered(&store, &r.id);
        assert_eq!(done.notify["a"].state, DeliveryState::Failed);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn the_header_comes_from_its_variable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("runs")).unwrap());
        let r = ended(&store, &[("a", DeliveryState::Pending)]);
        // SAFETY: a variable no other test reads.
        unsafe { std::env::set_var("FHIR_PIPES_TEST_NOTIFY_KEY", "k-123456") };
        let server = Server::run();
        server.expect(
            Expectation::matching(request::headers(contains(("x-api-key", "k-123456"))))
                .times(1)
                .respond_with(status_code(200)),
        );
        let mut d = destination("a", server.url_str("/i"));
        d.header = Some(("x-api-key".into(), "FHIR_PIPES_TEST_NOTIFY_KEY".into()));
        let n = Notifier::start(
            store.clone(),
            vec![d],
            Arc::default(),
            quick(),
            sink(),
            None,
        );
        n.enqueue(r.id.clone());
        assert_eq!(
            delivered(&store, &r.id).notify["a"].state,
            DeliveryState::Delivered
        );
    }
}
