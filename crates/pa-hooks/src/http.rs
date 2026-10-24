//! `http` hooks (Claude Code's): the event's input is posted as JSON to the
//! hook's `url`, and a 2xx response's body is read like a command hook's
//! stdout (an empty body is a plain success). Any other status, a transport
//! error or the timeout is a non-blocking failure (`onFailure: "block"`
//! turns it into a refusal where the event can be blocked).
//!
//! Policy, Claude Code's:
//! - `allowedHttpHookUrls` (every settings source's list, merged): when set,
//!   a URL must match one of its patterns (`*` matches any run of
//!   characters); an empty list allows none.
//! - Header values interpolate `$VAR` / `${VAR}` only for the names the
//!   hook's `allowedEnvVars` lists (narrowed by `httpHookAllowedEnvVars`
//!   when set); any other reference becomes empty. CR, LF and NUL are
//!   stripped from every value.
//! - No redirects are followed, and without a proxy configured in the
//!   environment a host that resolves to a private or link-local address is
//!   refused (loopback is allowed, for local development); the connection
//!   is pinned to the addresses checked.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use serde_json::Value;

use crate::config::{HookCommand, HookKind};
use crate::run::Outcome;

/// The HTTP hook policy every settings source contributes to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpPolicy {
    /// `allowedHttpHookUrls`; `None` allows every URL.
    pub allowed_urls: Option<Vec<String>>,
    /// `httpHookAllowedEnvVars`; `None` leaves each hook's own list as is.
    pub allowed_env_vars: Option<Vec<String>>,
}

impl HttpPolicy {
    /// Merge one settings document's lists into the policy.
    pub fn merge(&mut self, document: &serde_json::Map<String, Value>) {
        let strings = |key: &str| {
            document.get(key).and_then(Value::as_array).map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
        };
        if let Some(urls) = strings("allowedHttpHookUrls") {
            self.allowed_urls.get_or_insert_with(Vec::new).extend(urls);
        }
        if let Some(names) = strings("httpHookAllowedEnvVars") {
            self.allowed_env_vars
                .get_or_insert_with(Vec::new)
                .extend(names);
        }
    }

    fn url_allowed(&self, url: &str) -> bool {
        self.allowed_urls
            .as_ref()
            .is_none_or(|patterns| patterns.iter().any(|pattern| wildcard(pattern, url)))
    }
}

/// Whether `text` matches `pattern`, `*` matching any run of characters.
fn wildcard(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, tail)) => {
            let Some(rest) = text.strip_prefix(head) else {
                return false;
            };
            (0..=rest.len())
                .filter(|index| rest.is_char_boundary(*index))
                .any(|index| wildcard(tail, &rest[index..]))
        }
    }
}

/// Interpolate the allowed variables of `value` (`$VAR`, `${VAR}`) and strip
/// CR, LF and NUL.
fn interpolate(value: &str, allowed: &[String], lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(index) = rest.find('$') {
        out.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        let (name, consumed) = if let Some(braced) = after.strip_prefix('{') {
            match braced.find('}') {
                Some(end) => (&braced[..end], end + 2),
                None => ("", 0),
            }
        } else {
            let end = after
                .find(|ch: char| !(ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_'))
                .unwrap_or(after.len());
            (&after[..end], end)
        };
        let valid = name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_uppercase() || first == '_')
            && name
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_');
        if !valid || consumed == 0 {
            out.push('$');
            rest = after;
            continue;
        }
        if allowed.iter().any(|allowed| allowed == name) {
            out.push_str(&lookup(name).unwrap_or_default());
        } else {
            tracing::warn!(
                target: "pa_hooks",
                "env var ${name} is not in the http hook's allowedEnvVars; left empty"
            );
        }
        rest = &after[consumed..];
    }
    out.push_str(rest);
    out.retain(|ch| !matches!(ch, '\r' | '\n' | '\0'));
    out
}

/// Whether `address` is private or link-local (loopback is not).
fn is_private(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || a == 0
                // Shared address space (carrier-grade NAT).
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                !v4.is_loopback() && is_private(IpAddr::V4(v4))
            } else {
                let first = v6.segments()[0];
                v6.is_unspecified()
                    // Unique local (fc00::/7) and link-local (fe80::/10).
                    || (first & 0xfe00) == 0xfc00
                    || (first & 0xffc0) == 0xfe80
            }
        }
    }
}

/// Whether the environment configures a proxy (the request then goes
/// through it, and the address check is the proxy's business).
fn proxy_configured() -> bool {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .iter()
    .any(|name| std::env::var(name).is_ok_and(|value| !value.is_empty()))
}

fn failure(stderr: String) -> Outcome {
    Outcome {
        code: None,
        stdout: String::new(),
        stderr,
    }
}

/// POST `input` to `hook`'s URL.
pub async fn post(hook: &HookCommand, input: &Value) -> Outcome {
    let HookKind::Http {
        headers,
        allowed_env_vars,
        policy,
    } = &hook.kind
    else {
        return failure(format!("hook {:?} is not an http hook", hook.command));
    };
    let url = hook.command.as_str();
    if !policy.url_allowed(url) {
        return failure(format!(
            "HTTP hook blocked: {url} does not match any pattern in allowedHttpHookUrls"
        ));
    }
    let parsed = match reqwest::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(error) => return failure(format!("HTTP hook url {url:?}: {error}")),
    };
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(hook.timeout);
    // A loopback URL never goes through a proxy.
    let loopback = parsed.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if loopback || !proxy_configured() {
        let host = parsed.host_str().unwrap_or_default().to_string();
        let port = parsed.port_or_known_default().unwrap_or(80);
        let addresses: Vec<SocketAddr> = match tokio::net::lookup_host((host.as_str(), port)).await
        {
            Ok(addresses) => addresses.collect(),
            Err(error) => return failure(format!("HTTP hook error: {host}: {error}")),
        };
        if let Some(blocked) = addresses.iter().find(|address| is_private(address.ip())) {
            return failure(format!(
                "HTTP hook blocked: {host} resolves to {} (private/link-local address). \
                 Loopback (127.0.0.1, ::1) is allowed for local dev.",
                blocked.ip()
            ));
        }
        builder = builder.no_proxy().resolve_to_addrs(&host, &addresses);
    }
    let client = match builder.build() {
        Ok(client) => client,
        Err(error) => return failure(format!("HTTP hook error: {error}")),
    };
    let allowed: Vec<String> = match &policy.allowed_env_vars {
        Some(policy) => allowed_env_vars
            .iter()
            .filter(|name| policy.contains(name))
            .cloned()
            .collect(),
        None => allowed_env_vars.clone(),
    };
    let env = Arc::clone(&hook.env);
    let lookup = move |name: &str| {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var(name).ok())
    };
    let mut request = client
        .post(parsed)
        .header("Content-Type", "application/json")
        .body(input.to_string());
    for (name, value) in headers {
        request = request.header(name, interpolate(value, &allowed, &lookup));
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if status.is_success() {
                Outcome {
                    code: Some(0),
                    stdout: body,
                    stderr: String::new(),
                }
            } else {
                failure(format!("HTTP {} from {url}", status.as_u16()))
            }
        }
        Err(error) if error.is_timeout() => failure(format!(
            "HTTP hook {url:?} timed out after {}s",
            hook.timeout.as_secs_f64()
        )),
        Err(error) => failure(format!("HTTP hook error: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::config::{HookCommand, HookKind};

    #[test]
    fn urls_match_the_allowlist() {
        let policy = HttpPolicy {
            allowed_urls: Some(vec!["https://hooks.example.com/*".to_string()]),
            allowed_env_vars: None,
        };
        assert!(policy.url_allowed("https://hooks.example.com/pre"));
        assert!(!policy.url_allowed("https://evil.example.com/pre"));
        assert!(HttpPolicy::default().url_allowed("https://anything"));
        let none = HttpPolicy {
            allowed_urls: Some(Vec::new()),
            allowed_env_vars: None,
        };
        assert!(!none.url_allowed("https://hooks.example.com/pre"));
        let mut merged = HttpPolicy::default();
        merged.merge(json!({ "allowedHttpHookUrls": ["a"] }).as_object().unwrap());
        merged.merge(
            json!({ "allowedHttpHookUrls": ["b"], "httpHookAllowedEnvVars": ["T"] })
                .as_object()
                .unwrap(),
        );
        assert_eq!(
            merged,
            HttpPolicy {
                allowed_urls: Some(vec!["a".to_string(), "b".to_string()]),
                allowed_env_vars: Some(vec!["T".to_string()]),
            }
        );
    }

    #[test]
    fn headers_interpolate_only_allowed_variables() {
        let lookup = |name: &str| Some(format!("<{name}>"));
        let allowed = vec!["TOKEN".to_string()];
        assert_eq!(
            interpolate("Bearer $TOKEN and ${TOKEN}", &allowed, &lookup),
            "Bearer <TOKEN> and <TOKEN>"
        );
        assert_eq!(interpolate("x $SECRET y", &allowed, &lookup), "x  y");
        assert_eq!(
            interpolate("$5 and $ and ${", &allowed, &lookup),
            "$5 and $ and ${"
        );
        assert_eq!(interpolate("a\r\nb\0", &allowed, &lookup), "ab");
    }

    #[test]
    fn private_addresses_are_refused_and_loopback_is_not() {
        for private in [
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_private(private.parse().unwrap()), "{private}");
        }
        for public in [
            "127.0.0.1",
            "::1",
            "8.8.8.8",
            "2606:4700::1111",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_private(public.parse().unwrap()), "{public}");
        }
    }

    fn http_hook(url: &str, policy: HttpPolicy) -> HookCommand {
        HookCommand {
            command: url.to_string(),
            kind: HookKind::Http {
                headers: vec![("X-Token".to_string(), "$HOOK_TEST_TOKEN".to_string())],
                allowed_env_vars: vec!["HOOK_TEST_TOKEN".to_string()],
                policy: Arc::new(policy),
            },
            env: Arc::new(vec![("HOOK_TEST_TOKEN".to_string(), "t0k".to_string())]),
            ..HookCommand::shell("unused", Duration::from_secs(5))
        }
    }

    /// One HTTP exchange on a loopback listener: the request text it read
    /// and the response it sent.
    async fn serve_once(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
                if read == 0 {
                    break;
                }
            }
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
            String::from_utf8_lossy(&request).to_string()
        });
        (url, server)
    }

    #[tokio::test]
    async fn a_hook_posts_its_input_and_reads_the_body() {
        let body = r#"{"decision":"block","reason":"from http"}"#;
        let response: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let (url, server) = serve_once(response).await;
        let outcome = post(
            &http_hook(&url, HttpPolicy::default()),
            &json!({ "hook_event_name": "PreToolUse" }),
        )
        .await;
        assert_eq!(
            outcome,
            Outcome {
                code: Some(0),
                stdout: body.to_string(),
                stderr: String::new(),
            }
        );
        let request = server.await.unwrap();
        assert!(request.starts_with("POST /hook HTTP/1.1"), "{request}");
        let lower = request.to_ascii_lowercase();
        assert!(
            lower.contains("content-type: application/json"),
            "{request}"
        );
        assert!(lower.contains("x-token: t0k"), "{request}");
        assert!(
            request.ends_with(r#"{"hook_event_name":"PreToolUse"}"#),
            "{request}"
        );
    }

    #[tokio::test]
    async fn an_error_status_or_a_blocked_url_is_a_failure() {
        let (url, server) = serve_once(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let outcome = post(&http_hook(&url, HttpPolicy::default()), &json!({})).await;
        assert_eq!(outcome.code, None);
        assert_eq!(outcome.stderr, format!("HTTP 500 from {url}"));
        server.await.unwrap();
        let blocked = post(
            &http_hook(
                "http://127.0.0.1:9/hook",
                HttpPolicy {
                    allowed_urls: Some(vec!["https://hooks.example.com/*".to_string()]),
                    allowed_env_vars: None,
                },
            ),
            &json!({}),
        )
        .await;
        assert_eq!(blocked.code, None);
        assert!(
            blocked
                .stderr
                .contains("does not match any pattern in allowedHttpHookUrls"),
            "{blocked:?}"
        );
    }
}
