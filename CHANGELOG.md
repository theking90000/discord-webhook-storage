# Changelog

## 0.1.0 - 2026-10-01

- Stream file uploads to Discord webhooks through caller-supplied async transports
  with `WriteFile`, chunked request encoding, and vectored writes.
- Confirm uploads with `finish()` and return a `DiscordFile` reference. Enforce a
  file payload limit of 20,000,000 bytes.
- Stream full downloads or inclusive byte ranges with `ReadFile` and report
  interrupted downloads as errors.
- Serialize file references with Serde or the `discord://<id>/<url>` format.
  Parse signed attachment URLs and check their expiry against the local clock.
- Renew one file's URL with its webhook credentials, or renew URLs in place with
  bot credentials in batches of up to 50. Update duplicate URLs together.
- Parse length-delimited and chunked HTTP responses without `httparse`.
- Provide typed errors for credentials, file URLs, HTTP parsing, responses, and
  writes while preserving underlying error causes.
- Support borrowed connections and document upload finalization, connection
  reuse, URL expiry, and rate-limit handling.
- Provide TLS examples using the optional `tokio-tcp-pool` feature for uploads,
  range downloads, URL renewal, and concurrent operations.
- Add a Discord upload/download benchmark, its first throughput measurements,
  and the results of a Valgrind memory leak check.
- Add tests and CI checks for Rust 1.85 compatibility, feature configurations,
  documentation, and packaging.
