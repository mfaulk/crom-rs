mod headers;

pub use headers::Headers;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Delete,
    Options,
}

impl Method {
    pub fn parse(s: &str) -> Option<Method> {
        Some(match s {
            "GET" => Method::Get,
            "HEAD" => Method::Head,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "OPTIONS" => Method::Options,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Options => "OPTIONS",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Request {
    pub fn new(method: Method, path: &str) -> Self {
        Request {
            method,
            path: path.to_string(),
            headers: Headers::new(),
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.append(name, value);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Response {
            status,
            headers: Headers::new(),
            body: Vec::new(),
        }
    }

    pub fn text(status: u16, body: &str) -> Self {
        let mut r = Response::new(status);
        r.headers
            .insert("Content-Type", "text/plain; charset=utf-8");
        r.body = body.as_bytes().to_vec();
        r
    }
}
