# layercfg

Layered application configuration: built-in defaults, then a config file,
then environment variables. Later layers override earlier ones.

- `confval` — the value tree (`Value`) and the INI-style file parser
  (`[section.sub]` headers, `key = value` lines, `#` comments).
- `layers` — layer sources and merging; environment variables named
  `APP_<SECTION>__<KEY>` override `section.key` (`__` separates levels).
- `appconfig` — the typed `AppConfig` used by the service.
