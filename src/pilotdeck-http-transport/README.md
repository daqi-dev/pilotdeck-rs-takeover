# Shared product HTTP transport (work in progress)

This Kernel-independent product package is being extracted from the Gateway
`runtime/model/http.rs`, `http/tls.rs`, `http/encoding.rs`, and
`runtime/model/proxy_socks.rs` implementations. It retains the original CA,
proxy, SOCKS, content decoding and Hyper client behavior. A dial trait allows
Gateway's original ambient connector and the trusted Kernel-scoped host to use
one implementation; ordinary model-runtime and model-iface must not depend on
this package. Redirect policy is the shared model-runtime algorithm.

This draft is not yet added to the workspace, has not been built, and Gateway
still has its original implementation until its planned re-export migration.
Both copies must not be left as the final integrated state.

The migrated proxy replay fixture `tests/fixtures/mcp-proxy-schemes/userinfo-oracle.json`
is byte-identical to the Gateway fixture of the same name. Its SHA-256 is
`72be21de5f2edec5e81405228c18e70bbe66654c68239595cccd11bf8c4730e2`.
