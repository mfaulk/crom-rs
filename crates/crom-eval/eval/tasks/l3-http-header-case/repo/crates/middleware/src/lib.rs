//! Request/response layers.

use http_types::{Method, Request, Response};

pub trait Layer {
    /// Inspect or rewrite the request. Returning `Some` short-circuits the
    /// pipeline with that response.
    fn on_request(&self, _req: &mut Request) -> Option<Response> {
        None
    }

    fn on_response(&self, _req: &Request, _res: &mut Response) {}
}

/// Gzip/br negotiation. Real compression is out of scope; the layer marks the
/// chosen coding in `Content-Encoding` and prefixes the body so callers can
/// observe the decision.
pub struct Compression {
    pub min_size: usize,
}

impl Compression {
    pub fn negotiate(accept: &str) -> Option<&'static str> {
        let mut best: Option<(&'static str, f32)> = None;
        for part in accept.split(',') {
            let mut it = part.trim().split(';');
            let coding = it.next().unwrap_or("").trim();
            let q = it
                .find_map(|p| p.trim().strip_prefix("q="))
                .and_then(|q| q.parse::<f32>().ok())
                .unwrap_or(1.0);
            let name = match coding {
                "br" => "br",
                "gzip" => "gzip",
                _ => continue,
            };
            if q > 0.0 && best.map_or(true, |(_, bq)| q > bq) {
                best = Some((name, q));
            }
        }
        best.map(|(n, _)| n)
    }
}

impl Layer for Compression {
    fn on_response(&self, req: &Request, res: &mut Response) {
        if res.body.len() < self.min_size || res.headers.contains("content-encoding") {
            return;
        }
        let Some(accept) = req.headers.get("accept-encoding") else {
            return;
        };
        if let Some(coding) = Compression::negotiate(accept) {
            res.headers.insert("Content-Encoding", coding);
            res.headers.append("Vary", "Accept-Encoding");
            let mut body = format!("[{coding}]").into_bytes();
            body.extend_from_slice(&res.body);
            res.body = body;
        }
    }
}

/// Stamp `Content-Length` from the final body.
pub struct ContentLength;

impl Layer for ContentLength {
    fn on_response(&self, req: &Request, res: &mut Response) {
        if req.method == Method::Head {
            return;
        }
        res.headers
            .insert("Content-Length", &res.body.len().to_string());
    }
}

/// Permissive CORS for an allow-list of origins.
pub struct Cors {
    pub allowed_origins: Vec<String>,
}

impl Layer for Cors {
    fn on_request(&self, req: &mut Request) -> Option<Response> {
        if req.method != Method::Options {
            return None;
        }
        let origin = req.headers.get("Origin")?;
        let mut res = Response::new(204);
        if self.allowed_origins.iter().any(|o| o == origin) {
            res.headers.insert("Access-Control-Allow-Origin", origin);
            res.headers
                .insert("Access-Control-Allow-Methods", "GET, POST, PUT, DELETE");
        }
        Some(res)
    }

    fn on_response(&self, req: &Request, res: &mut Response) {
        if let Some(origin) = req.headers.get("Origin") {
            if self.allowed_origins.iter().any(|o| o == origin) {
                res.headers.insert("Access-Control-Allow-Origin", origin);
                res.headers.append("Vary", "Origin");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_prefers_highest_q() {
        assert_eq!(Compression::negotiate("gzip;q=0.5, br;q=0.8"), Some("br"));
        assert_eq!(Compression::negotiate("gzip, br;q=0"), Some("gzip"));
        assert_eq!(Compression::negotiate("identity"), None);
    }

    #[test]
    fn compresses_large_bodies() {
        let req = Request::new(Method::Get, "/").with_header("accept-encoding", "gzip");
        let mut res = Response::text(200, &"x".repeat(64));
        Compression { min_size: 32 }.on_response(&req, &mut res);
        assert_eq!(res.headers.get("Content-Encoding"), Some("gzip"));
        assert!(res.body.starts_with(b"[gzip]"));
    }
}
