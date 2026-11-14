use http_types::{Method, Request, Response};
use middleware::{Compression, ContentLength, Cors};
use server::Pipeline;

fn app() -> Pipeline<impl Fn(&Request) -> Response> {
    Pipeline::new(|req: &Request| match req.path.as_str() {
        "/big" => Response::text(200, &"lorem ipsum ".repeat(20)),
        _ => Response::text(200, "ok"),
    })
    .layer(ContentLength)
    .layer(Compression { min_size: 100 })
    .layer(Cors {
        allowed_origins: vec!["https://app.example".into()],
    })
}

#[test]
fn small_body_untouched() {
    let res = app().call(Request::new(Method::Get, "/").with_header("accept-encoding", "gzip"));
    assert_eq!(res.body, b"ok");
    assert_eq!(res.headers.get("Content-Length"), Some("2"));
}

#[test]
fn large_body_compressed_and_length_matches() {
    let res = app().call(Request::new(Method::Get, "/big").with_header("accept-encoding", "br"));
    assert_eq!(res.headers.get("Content-Encoding"), Some("br"));
    let len: usize = res.headers.get("Content-Length").unwrap().parse().unwrap();
    assert_eq!(len, res.body.len());
}

#[test]
fn cors_preflight() {
    let res = app()
        .call(Request::new(Method::Options, "/big").with_header("Origin", "https://app.example"));
    assert_eq!(res.status, 204);
    assert_eq!(
        res.headers.get("Access-Control-Allow-Origin"),
        Some("https://app.example")
    );
}
