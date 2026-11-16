# mdtoc

Generates a table of contents for Markdown documents.

- `mdscan` — finds ATX headings (`#` … `######`).
- `slugger` — turns heading text into URL fragment anchors.
- `toc` — renders a nested bullet list of links and can splice it between
  `<!-- toc -->` / `<!-- /toc -->` markers.
