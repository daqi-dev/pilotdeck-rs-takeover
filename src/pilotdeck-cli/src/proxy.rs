//! The console side of the baseline's global proxy install
//! (src/cli/proxy.ts `installGlobalProxy`): `pilotdeck.ts:28` runs it for
//! every command before `main()`, from the environment; `pilotdeck server`
//! runs it a second time with `proxy.url` from `pilotdeck.yaml`
//! (pilotdeck.ts:48-50), which only takes effect when no environment proxy
//! was installed. Each successful install logs the proxy and its effective
//! `noProxy`; a proxy URL undici's `EnvHttpProxyAgent` rejects logs a warning
//! and leaves direct transport in place.
//!
//! Which requests go through the proxy is decided by the gateway's model
//! transport (`pilotdeck_gateway` `runtime::model::http`), which reads the same
//! environment and configuration; this module only reproduces the output.
use super::paths::Env;
use super::{stderr_line, stdout_line};
use std::sync::atomic::{AtomicBool, Ordering};

/// `dispatcherState.mode === "proxy" && source === "env"`.
static ENV_PROXY_INSTALLED: AtomicBool = AtomicBool::new(false);

/// `getProxyUrl` (proxy.ts:20-28): the first non-empty of the five variables.
fn env_proxy_url(env: &Env) -> Option<String> {
    [
        "PILOTDECK_PROXY",
        "https_proxy",
        "HTTPS_PROXY",
        "http_proxy",
        "HTTP_PROXY",
    ]
    .into_iter()
    .filter_map(|name| env.string(name))
    .find(|value| !value.is_empty())
}

/// The top-level `await installGlobalProxy()` (pilotdeck.ts:28).
pub fn install_from_env(env: &Env) {
    if let Some(url) = env_proxy_url(env)
        && install(env, &url)
    {
        ENV_PROXY_INSTALLED.store(true, Ordering::SeqCst);
    }
}

/// `installGlobalProxy(snapshot.config.proxy.url)` in the server branch: an
/// environment proxy already installed keeps precedence (proxy.ts:64-70).
pub fn install_from_config(env: &Env, url: &str) {
    if !ENV_PROXY_INSTALLED.load(Ordering::SeqCst) {
        install(env, url);
    }
}

/// `applyGlobalProxy` (proxy.ts:122-147). Returns whether the install
/// succeeded.
fn install(env: &Env, url: &str) -> bool {
    match validate(url) {
        Ok(()) => {
            // buildNoProxy (proxy.ts:163-168) reads the process environment.
            let user = env
                .string("no_proxy")
                .filter(|value| !value.is_empty())
                .or_else(|| env.string("NO_PROXY"))
                .unwrap_or_default();
            let no_proxy = [user.as_str(), "127.0.0.1", "localhost"]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(",");
            stdout_line(&format!(
                "[proxy] Global fetch proxy \u{2192} {url} (noProxy: {no_proxy})"
            ));
            true
        }
        Err(message) => {
            stderr_line(&format!(
                "[proxy] Failed to install global proxy ({url}): {message}"
            ));
            false
        }
    }
}

/// The checks `new EnvHttpProxyAgent({ httpProxy: url, ... })` makes, in the
/// order its `ProxyAgent` constructor makes them, with their messages: the
/// WHATWG URL parse, `decodeURIComponent` of the userinfo, then undici's
/// `parseURL` (protocol, then no path / query / fragment).
fn validate(url: &str) -> Result<(), &'static str> {
    let parsed = url::Url::parse(url).map_err(|_| "Invalid URL")?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        decode_uri_component(parsed.username()).ok_or("URI malformed")?;
        decode_uri_component(parsed.password().unwrap_or("")).ok_or("URI malformed")?;
    }
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("Invalid URL protocol: the URL must start with `http:` or `https:`.");
    }
    if parsed.path() != "/"
        || parsed.query().is_some_and(|query| !query.is_empty())
        || parsed
            .fragment()
            .is_some_and(|fragment| !fragment.is_empty())
    {
        return Err("invalid url");
    }
    Ok(())
}

/// `decodeURIComponent`: `None` where it throws `URIError: URI malformed`
/// (a broken `%XX` escape or a decoded byte sequence that is not UTF-8).
fn decode_uri_component(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = input.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_follows_the_proxy_agent_constructor_order() {
        assert_eq!(validate("http://proxy.invalid:3128"), Ok(()));
        assert_eq!(validate("http://proxy.invalid:3128/?"), Ok(()));
        assert_eq!(validate("not a url"), Err("Invalid URL"));
        assert_eq!(validate("http://%zz@p.invalid:1"), Err("URI malformed"));
        // userinfo is decoded before the protocol is checked
        assert_eq!(validate("ftp://%zz@p.invalid:1"), Err("URI malformed"));
        assert_eq!(
            validate("socks5://p.invalid:1"),
            Err("Invalid URL protocol: the URL must start with `http:` or `https:`.")
        );
        assert_eq!(validate("http://p.invalid:8/sub"), Err("invalid url"));
        assert_eq!(validate("http://p.invalid:8/?q=1"), Err("invalid url"));
        assert_eq!(validate("http://u:p%20w@p.invalid:8"), Ok(()));
        assert_eq!(validate("http://u:%ff@p.invalid:8"), Err("URI malformed"));
    }
}
