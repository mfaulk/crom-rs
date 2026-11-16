# httpkit

Transport-free HTTP building blocks.

- `http-types` — `Method`, `Request`, `Response`, and the `Headers` map.
- `middleware` — composable request/response layers: content negotiation for
  compression, `Content-Length` stamping, CORS.
- `server` — a pipeline that runs middleware around a handler, plus a
  minimal HTTP/1.1 request parser.
