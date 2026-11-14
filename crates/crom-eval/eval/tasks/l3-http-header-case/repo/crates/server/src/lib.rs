//! A middleware pipeline and a minimal HTTP/1.1 request parser.

use http_types::{Method, Request, Response};
use middleware::Layer;

pub struct Pipeline<H> {
    layers: Vec<Box<dyn Layer>>,
    handler: H,
}

impl<H: Fn(&Request) -> Response> Pipeline<H> {
    pub fn new(handler: H) -> Self {
        Pipeline {
            layers: Vec::new(),
            handler,
        }
    }

    pub fn layer(mut self, layer: impl Layer + 'static) -> Self {
        self.layers.push(Box::new(layer));
        self
    }

    /// Run request layers in order, the handler, then response layers in
    /// reverse order.
    pub fn call(&self, mut req: Request) -> Response {
        let mut ran = 0;
        let mut short = None;
        for layer in &self.layers {
            ran += 1;
            if let Some(res) = layer.on_request(&mut req) {
                short = Some(res);
                break;
            }
        }
        let mut res = short.unwrap_or_else(|| (self.handler)(&req));
        for layer in self.layers[..ran].iter().rev() {
            layer.on_response(&req, &mut res);
        }
        res
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    BadRequestLine,
    BadHeader(String),
    UnsupportedMethod(String),
}

/// Parse a request head (`\r\n`-separated) plus body.
pub fn parse_request(raw: &[u8]) -> Result<Request, ParseError> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h.to_string(), b.as_bytes().to_vec()),
        None => (text.to_string(), Vec::new()),
    };
    let mut lines = head.split("\r\n");
    let line = lines.next().ok_or(ParseError::BadRequestLine)?;
    let mut parts = line.split(' ');
    let (m, path, version) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(p), Some(v)) => (m, p, v),
        _ => return Err(ParseError::BadRequestLine),
    };
    if !version.starts_with("HTTP/1.") {
        return Err(ParseError::BadRequestLine);
    }
    let method = Method::parse(m).ok_or_else(|| ParseError::UnsupportedMethod(m.to_string()))?;
    let mut req = Request::new(method, path);
    for l in lines {
        let (name, value) = l
            .split_once(':')
            .ok_or_else(|| ParseError::BadHeader(l.to_string()))?;
        if name.is_empty() || name.contains(' ') {
            return Err(ParseError::BadHeader(l.to_string()));
        }
        req.headers.append(name, value.trim());
    }
    req.body = body;
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request() {
        let req =
            parse_request(b"POST /items HTTP/1.1\r\nhost: x\r\naccept-encoding: br\r\n\r\nhi")
                .unwrap();
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.path, "/items");
        assert_eq!(req.headers.get("host"), Some("x"));
        assert_eq!(req.body, b"hi");
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_request(b"HELLO"), Err(ParseError::BadRequestLine));
        assert_eq!(
            parse_request(b"BREW /pot HTTP/1.1\r\n\r\n"),
            Err(ParseError::UnsupportedMethod("BREW".into()))
        );
    }
}
