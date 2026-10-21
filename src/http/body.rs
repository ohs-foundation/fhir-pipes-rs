//! Reading response bodies: gzip decoded with `MultiGzDecoder`, whole
//! bodies capped at the client's limit once decoded, and error bodies cut
//! short.

use std::io::Read;

use reqwest::blocking::Response;
use reqwest::header::CONTENT_ENCODING;

use super::{Exchange, FetchError, Fetched};

const ERROR_BODY_CAP: usize = 500;

/// Never read more than this much of an error body off the wire; we only
/// keep the first ERROR_BODY_CAP chars of it anyway.
const ERROR_BODY_READ_LIMIT: u64 = 8192;

/// How much of a non-2xx body `exchange` keeps whole, for a caller that
/// parses it (a Bulk Data OperationOutcome). Beyond it the body is cut and
/// `truncated` set.
const EXCHANGE_BODY_LIMIT: u64 = 1 << 20;

/// The most a 2xx body read whole may decode to: far more than a search
/// page, a boundary or a package holds.
pub const DEFAULT_BODY_LIMIT: u64 = 1 << 30;

/// A response's body as it was sent, before `Content-Encoding: gzip`.
/// `MultiGzDecoder` reads every member: concatenated gzip members are one
/// valid gzip body (RFC 1952 section 2.2), and a decoder that stops after
/// the first loses the rest without an error. Any other encoding is passed
/// through as it came; the client only asks for gzip. An empty body is
/// empty whatever its `Content-Encoding` says (a 202 from a server that
/// marks every response gzip), as reqwest's decoder treated it.
pub(super) fn decoded(resp: Response) -> Box<dyn Read> {
    use std::io::BufRead as _;
    let gzip = resp
        .headers()
        .get(CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            let v = v.trim();
            v.eq_ignore_ascii_case("gzip") || v.eq_ignore_ascii_case("x-gzip")
        });
    if !gzip {
        return Box::new(resp);
    }
    let mut body = std::io::BufReader::new(resp);
    // After a read error here the decoder fails too: the same error again,
    // or a truncated stream.
    if body.fill_buf().is_ok_and(|b| b.is_empty()) {
        return Box::new(std::io::empty());
    }
    Box::new(flate2::bufread::MultiGzDecoder::new(body))
}

/// The whole decoded body, failing past `limit` bytes: a small gzip body can
/// decode to far more than memory holds.
fn read_body(resp: Response, limit: u64) -> std::result::Result<Vec<u8>, FetchError> {
    let mut body = Vec::new();
    decoded(resp)
        .take(limit.saturating_add(1))
        .read_to_end(&mut body)
        .map_err(|e| FetchError::Transport(format!("reading body: {e}")))?;
    if body.len() as u64 > limit {
        return Err(FetchError::Transport(format!(
            "the response is larger than {} MB once decoded; not read",
            limit >> 20
        )));
    }
    Ok(body)
}

/// A 2xx response's body and `Date`.
pub(super) fn fetched(resp: Response, limit: u64) -> std::result::Result<Fetched, FetchError> {
    let date = resp
        .headers()
        .get(reqwest::header::DATE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Ok(Fetched {
        body: read_body(resp, limit)?,
        date,
    })
}

/// Any response, with its status and headers.
pub(super) fn exchanged(resp: Response, limit: u64) -> std::result::Result<Exchange, FetchError> {
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let mut truncated = false;
    let body = if (200..300).contains(&status) {
        read_body(resp, limit)?
    } else {
        let mut buf = Vec::new();
        let _ = decoded(resp)
            .take(EXCHANGE_BODY_LIMIT + 1)
            .read_to_end(&mut buf);
        if buf.len() as u64 > EXCHANGE_BODY_LIMIT {
            buf.truncate(EXCHANGE_BODY_LIMIT as usize);
            truncated = true;
        }
        buf
    };
    Ok(Exchange {
        status,
        headers,
        body,
        truncated,
    })
}

fn cap(body: String) -> String {
    body.chars().take(ERROR_BODY_CAP).collect()
}

/// Read at most ERROR_BODY_READ_LIMIT bytes of an error response, then cap
/// to ERROR_BODY_CAP chars. Bounded so a server that streams gigabytes of
/// "error" doesn't make us buffer it all just to report a status code.
pub(super) fn read_error_body(resp: Response) -> String {
    let mut buf = Vec::new();
    let _ = decoded(resp)
        .take(ERROR_BODY_READ_LIMIT)
        .read_to_end(&mut buf);
    cap(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::time::Duration;

    use reqwest::Method;

    use crate::http::tests::client;
    use httptest::{Expectation, Server, matchers::*, responders::*};

    pub(crate) fn gzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    /// `parts` as one gzip body of a member each (RFC 1952 section 2.2).
    pub(crate) fn gzip_members(parts: &[&[u8]]) -> Vec<u8> {
        parts.iter().flat_map(|p| gzip(p)).collect()
    }

    #[test]
    fn every_member_of_a_multi_member_gzip_page_is_read() {
        let page = br#"{"resourceType":"Bundle","type":"searchset","total":2}"#;
        let (a, b) = page.split_at(20);
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/fhir/Patient"))
                .times(1)
                .respond_with(
                    status_code(200)
                        .insert_header("content-encoding", "gzip")
                        .body(gzip_members(&[a, b])),
                ),
        );
        let got = client(1).get(&server.url_str("/fhir/Patient")).unwrap();
        assert_eq!(got.body, page);
    }

    #[test]
    fn a_server_that_ignores_accept_encoding_is_read_as_sent() {
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/plain"),
                request::headers(contains(("accept-encoding", matches("gzip")))),
            ])
            .times(1)
            .respond_with(status_code(200).body("plain text")),
        );
        let got = client(1).get(&server.url_str("/plain")).unwrap();
        assert_eq!(got.body, b"plain text");
    }

    #[test]
    fn an_empty_gzip_body_and_a_gzipped_error_are_decoded() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/empty"))
                .times(1)
                .respond_with(status_code(202).insert_header("content-encoding", "gzip")),
        );
        server.expect(
            Expectation::matching(request::method_path("GET", "/missing"))
                .times(1)
                .respond_with(
                    status_code(404)
                        .insert_header("content-encoding", "gzip")
                        .body(gzip_members(&[b"no such ", b"resource"])),
                ),
        );
        let c = client(1);
        let got = c.exchange(Method::GET, &server.url_str("/empty")).unwrap();
        assert_eq!((got.status, got.body.len()), (202, 0));
        let err = c.get(&server.url_str("/missing")).unwrap_err();
        assert!(
            matches!(err, FetchError::Status { status: 404, ref body } if body == "no such resource"),
            "{err:?}"
        );
    }

    #[test]
    fn a_corrupt_gzip_body_is_an_error() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/bad"))
                .times(1)
                .respond_with(
                    status_code(200)
                        .insert_header("content-encoding", "gzip")
                        .body("not gzip"),
                ),
        );
        let err = client(1).get(&server.url_str("/bad")).unwrap_err();
        assert!(matches!(err, FetchError::Transport(_)), "{err:?}");
    }

    #[test]
    fn every_member_of_a_gzipped_download_reaches_the_reader() {
        let lines = "{\"resourceType\":\"Patient\",\"id\":\"a\"}\n".repeat(1000);
        let (head, tail) = lines.as_bytes().split_at(lines.len() / 2);
        let server = Server::run();
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", "/files/p.ndjson"),
                request::headers(contains(("accept", "application/fhir+ndjson"))),
                request::headers(contains(("accept-encoding", matches("gzip")))),
            ])
            .times(1)
            .respond_with(
                status_code(200)
                    .insert_header("content-encoding", "gzip")
                    .body(gzip_members(&[head, tail])),
            ),
        );
        let got = client(1)
            .download(
                &server.url_str("/files/p.ndjson"),
                false,
                Duration::from_secs(5),
                |body| {
                    let mut text = String::new();
                    body.read_to_string(&mut text)?;
                    Ok(text)
                },
            )
            .unwrap();
        assert_eq!(got, lines);
    }

    #[test]
    fn a_body_that_decodes_past_the_limit_is_not_read() {
        let server = Server::run();
        // 2 MB of zeros, a few KB once gzipped.
        server.expect(
            Expectation::matching(request::method_path("GET", "/big")).respond_with(
                status_code(200)
                    .insert_header("content-encoding", "gzip")
                    .body(gzip_members(&[&vec![0u8; 2 << 20]])),
            ),
        );
        let err = client(1)
            .with_body_limit(1 << 20)
            .get(&server.url_str("/big"))
            .unwrap_err();
        assert!(err.to_string().contains("larger than 1 MB"), "{err}");
    }

    #[test]
    fn a_body_larger_than_the_cap_is_truncated_in_the_error() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", "/big"))
                .respond_with(status_code(400).body("x".repeat(2000))),
        );
        let err = client(1).get(&server.url("/big").to_string()).unwrap_err();
        match err {
            FetchError::Status { body, .. } => assert_eq!(body.chars().count(), 500),
            other => panic!("{other:?}"),
        }
    }
}
