use http_types::{Method, Request, Response};
use middleware::{Compression, ContentLength, Cors};
use server::{parse_request, Pipeline};

fn app() -> Pipeline<impl Fn(&Request) -> Response> {
    Pipeline::new(|req: &Request| match req.path.as_str() {
        "/big" => Response::text(200, &"lorem ipsum ".repeat(20)),
        "/manual" => {
            let mut r = Response::text(200, "hand-made");
            r.headers.insert("content-length", "999");
            r
        }
        "/pre" => {
            let mut r = Response::text(200, &"z".repeat(300));
            r.headers.insert("content-encoding", "identity");
            r
        }
        _ => Response::text(200, "ok"),
    })
    .layer(ContentLength)
    .layer(Compression { min_size: 100 })
    .layer(Cors {
        allowed_origins: vec!["https://app.example".into()],
    })
}

#[test]
fn real_client_request_is_compressed_with_cors() {
    let raw = b"GET /big HTTP/1.1\r\nHost: api\r\nAccept-Encoding: gzip, br;q=0.5\r\nOrigin: https://app.example\r\n\r\n";
    let res = app().call(parse_request(raw).unwrap());
    assert_eq!(res.headers.get("Content-Encoding"), Some("gzip"));
    assert_eq!(
        res.headers.get("access-control-allow-origin"),
        Some("https://app.example")
    );
}

#[test]
fn single_content_length() {
    let res = app().call(Request::new(Method::Get, "/manual"));
    let lens = res.headers.get_all("Content-Length");
    assert_eq!(lens.len(), 1, "{lens:?}");
    assert_eq!(lens[0], res.body.len().to_string());
}

#[test]
fn existing_encoding_not_overridden() {
    let res = app().call(Request::new(Method::Get, "/pre").with_header("ACCEPT-ENCODING", "br"));
    assert_eq!(res.headers.get_all("content-encoding"), ["identity"]);
}

#[test]
fn coding_tokens_ignore_case() {
    let res = app().call(Request::new(Method::Get, "/big").with_header("Accept-Encoding", "GZIP"));
    assert_eq!(res.headers.get("content-encoding"), Some("gzip"));
    assert!(res.body.starts_with(b"[gzip]"));
}

#[test]
fn preflight_with_lowercase_origin() {
    let res = app()
        .call(Request::new(Method::Options, "/x").with_header("origin", "https://app.example"));
    assert_eq!(res.status, 204);
    assert!(res.headers.contains("Access-Control-Allow-Methods"));
}
