# Trusted HTTP host extraction (work in progress)

This directory is an unconnected crate draft, not a working managed transport.
The socket, proxy, TLS, SOCKS and decoding implementation now lives in the
Kernel-independent `pilotdeck-http-transport` package. This crate contains
the trusted Kernel dial bridge, pull projection, scope lifecycle, and a
re-export of the single pure model-core credential helpers. It is not wired
into the workspace and lacks the approved provider binding, two-stage header
preflight, and completed validation. Do not use it until those are implemented.

The shared HTTP implementation was extracted from Gateway
`runtime/model/http.rs`, `http/{tls,encoding}.rs`, and `proxy_socks.rs`.
The redirect algorithm is shared via `pilotdeck-model-runtime::fetch_redirect`;
no HTTP implementation is intended to remain duplicated after Gateway is wired.
The scoped bridge performs managed authorization for both direct and proxy
routes; the existing Gateway adapter retains its original ambient behavior.

Each attempt's receipt must reflect actual resource ownership. A normal framed
EOF may become Released once that attempt's body, buffers and helper have
converged. After early stop, error or drop, the high-level Hyper client does not
provide a proven per-attempt cancellation/lease-return acknowledgement: keep
resources Held or Unknown until the owning job's pool is shut down and its
tracked connection drivers, connects and helpers have all finished. The local
stop must not cancel the job or other attempts; a pending receipt must not by
itself suppress a retry that product policy otherwise permits. Pool shutdown
must have observable completion, not merely drop the client handle.

Pools are partitioned by host-minted job/generation scope, with reuse inside
that scope. This differs from the original potentially cross-job pool and must
be validated for connection count, initial response latency and concurrent
resource bounds. Each dispatch rechecks authorization even if a connection is
reused; each new connection independently passes the Kernel's managed dialer.

Credentials must come from the job's trusted provider binding, never a
provider/slot asserted by the request. The host must reuse the model core's
existing `*_with_auth` header construction at the original merge point, then
verify the secret-free projection. This preserves custom-header precedence,
case-insensitive duplicates, insertion position and omission semantics; it
must also enforce the approved origin on every redirect. Simply appending an
Authorization header to the flattened request does not satisfy this contract.

The original header pipeline has two distinct timing points: credential/header
materialization (and Google API-key coercion) precedes encoding/dump; after the
dump, `build_http_request` parses the URL and validates the full header record.
Any trusted preflight must preserve both stages and their error precedence,
return only secret-free projections, and reuse the shared original algorithms.

A redirect Location can itself contain a registered secret. The chain keeps
its real and sanitized locations separately and authorizes the real target.
If sanitization makes their cross-origin decisions disagree about removal of
sensitive headers, the next hop is rejected without I/O. This is a deliberate
fail-closed difference from the ambient Gateway path and must be represented in
integration evidence; a same-origin secret query should still reach its real,
approved target without exposing it to the ordinary driver.

Before any preparation that can return an error, the same trusted registry
must already contain provider credentials, custom authentication headers and
proxy credential derivatives. Original product errors as well as typed host
rejections must be filtered before crossing the boundary. Preparation tests
must combine an invalid secret-bearing header with an invalid URL to verify
both non-disclosure and the original error precedence, without moving either
validation to an earlier stage.

The trusted composition API has no default credential or network allow. It
requires the real project lease, frozen provider and safe view, Host-verified
binding, explicitly approved origins, canonical model and approved post-override
wire model. Setup rejects an incomplete known-secret registry before creating a
job header service; this security rejection can precede an otherwise later
product validation error. Custom secret fields must also be registered by the
trusted project snapshot supplier. These guarantees have not been validated
against the production composition path yet.
