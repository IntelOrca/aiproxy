//! Listen address handling for the `-b`/`--bind` option.
//!
//! A bind value is a host with an optional port: `localhost`, `127.0.0.1:8080`,
//! `*`, `*:9090`, `:9090` (both meaning every interface) or a bracketed IPv6
//! literal (`[::1]`, `[::1]:9090`). The port a bind value carries wins over the
//! configured port, while a `-p`/`--port` that disagrees with it is a
//! contradiction rather than an override.

/// Host bound when a bind value names no host (`*:9090`, `:9090`).
pub const WILDCARD_HOST: &str = "0.0.0.0";

/// A parsed `-b`/`--bind` value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindAddress {
    /// Host to listen on; `*` and an omitted host mean [`WILDCARD_HOST`].
    pub host: String,
    /// Port the value carried, if any.
    pub port: Option<u16>,
}

/// Host for a config or bind value, mapping `*` and an empty host to the
/// wildcard address.
pub fn normalize_host(host: &str) -> String {
    let host = host.trim();
    if host.is_empty() || host == "*" {
        WILDCARD_HOST.to_string()
    } else {
        host.to_string()
    }
}

/// Parse a `-b`/`--bind` value into a host and an optional port.
pub fn parse_bind(raw: &str) -> Result<BindAddress, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty bind address".to_string());
    }

    let (host, port) = match raw.strip_prefix('[') {
        // Bracketed IPv6 literal, with or without a port: `[::1]`, `[::1]:9090`.
        Some(rest) => {
            let end = rest
                .find(']')
                .ok_or_else(|| format!("invalid bind address \"{raw}\": missing ']'"))?;
            let host = &rest[..end];
            if host.parse::<std::net::IpAddr>().is_err() {
                return Err(format!(
                    "invalid bind address \"{raw}\": \"{host}\" is not an IP address"
                ));
            }
            let tail = rest[end + 1..].trim();
            if tail.is_empty() {
                (host.to_string(), None)
            } else if let Some(port) = tail.strip_prefix(':') {
                (host.to_string(), Some(parse_port(port, raw)?))
            } else {
                return Err(format!("invalid bind address \"{raw}\""));
            }
        }
        // A bare IP literal carries no port; anything else is `host:port`.
        None if raw.parse::<std::net::IpAddr>().is_ok() => (raw.to_string(), None),
        None => match raw.split_once(':') {
            Some((host, port)) => (normalize_host(host), Some(parse_port(port, raw)?)),
            None => (normalize_host(raw), None),
        },
    };

    Ok(BindAddress { host, port })
}

/// Parse the port part of a bind value, reporting the offending input.
fn parse_port(port: &str, raw: &str) -> Result<u16, String> {
    let port = port.trim();
    port.parse::<u16>()
        .map_err(|_| format!("invalid port \"{port}\" in bind address \"{raw}\""))
}

/// Combine the configured listen address with the `-b`/`--bind` and
/// `-p`/`--port` overrides, returning the host and port to listen on.
///
/// A bind value without a port leaves the configured port in place; one with a
/// port overrides it. A `-p`/`--port` that names a different port than the bind
/// value is a contradiction and fails.
pub fn resolve_listen(
    host: &str,
    port: u16,
    bind: Option<&str>,
    port_override: Option<u16>,
) -> Result<(String, u16), String> {
    let Some(raw) = bind else {
        return Ok((host.to_string(), port_override.unwrap_or(port)));
    };
    let bind = parse_bind(raw)?;
    match bind.port {
        Some(bind_port) => match port_override {
            Some(cli_port) if cli_port != bind_port => Err(format!(
                "--bind {raw} sets port {bind_port}, but --port {cli_port} was also given"
            )),
            _ => Ok((bind.host, bind_port)),
        },
        None => Ok((bind.host, port_override.unwrap_or(port))),
    }
}

/// Combine the configured listen address, a `bind` value from the config, and
/// the `-b`/`--bind` and `-p`/`--port` overrides, returning the host and port
/// to listen on.
///
/// The config bind is resolved first: its port, if it carries one, replaces
/// the configured `port`. The command line is then applied on top, so `-b`
/// and `-p` always override the config. A `-p`/`--port` that names a different
/// port than a `-b`/`--bind` value is a contradiction and fails.
pub fn resolve_listen_from_config(
    host: &str,
    port: u16,
    config_bind: Option<&str>,
    cli_bind: Option<&str>,
    port_override: Option<u16>,
) -> Result<(String, u16), String> {
    let (host, port) = resolve_listen(host, port, config_bind, None)
        .map_err(|message| format!("invalid bind in the config: {message}"))?;
    resolve_listen(&host, port, cli_bind, port_override)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(host: &str, port: Option<u16>) -> BindAddress {
        BindAddress {
            host: host.to_string(),
            port,
        }
    }

    #[test]
    fn a_bare_host_keeps_the_configured_port() {
        assert_eq!(parse_bind("localhost").unwrap(), bind("localhost", None));
        assert_eq!(parse_bind(" 127.0.0.1 ").unwrap(), bind("127.0.0.1", None));
        assert_eq!(parse_bind("0.0.0.0").unwrap(), bind(WILDCARD_HOST, None));
        assert_eq!(parse_bind("*").unwrap(), bind(WILDCARD_HOST, None));
    }

    #[test]
    fn a_host_and_port_are_both_taken() {
        assert_eq!(
            parse_bind("localhost:9090").unwrap(),
            bind("localhost", Some(9090))
        );
        assert_eq!(
            parse_bind("*:9090").unwrap(),
            bind(WILDCARD_HOST, Some(9090))
        );
        assert_eq!(
            parse_bind(":9090").unwrap(),
            bind(WILDCARD_HOST, Some(9090))
        );
        assert_eq!(
            parse_bind("192.168.1.9:8080").unwrap(),
            bind("192.168.1.9", Some(8080))
        );
    }

    #[test]
    fn ipv6_literals_need_brackets_for_a_port() {
        assert_eq!(parse_bind("::1").unwrap(), bind("::1", None));
        assert_eq!(parse_bind("[::1]").unwrap(), bind("::1", None));
        assert_eq!(parse_bind("[::1]:9090").unwrap(), bind("::1", Some(9090)));
        assert_eq!(parse_bind("[::]:9090").unwrap(), bind("::", Some(9090)));
    }

    #[test]
    fn invalid_bind_values_are_rejected() {
        for raw in ["", "   ", "localhost:abc", "localhost:99999", "host:1:2"] {
            assert!(parse_bind(raw).is_err(), "{raw:?} should not parse");
        }
        assert!(parse_bind("[::1").is_err());
        assert!(parse_bind("[::1]9090").is_err());
        assert!(parse_bind("[nope]:9090").is_err());
    }

    #[test]
    fn no_bind_leaves_the_configured_address_alone() {
        assert_eq!(
            resolve_listen("127.0.0.1", 8080, None, None).unwrap(),
            ("127.0.0.1".to_string(), 8080)
        );
        // Without -b, -p is a plain override.
        assert_eq!(
            resolve_listen("127.0.0.1", 8080, None, Some(9090)).unwrap(),
            ("127.0.0.1".to_string(), 9090)
        );
    }

    #[test]
    fn a_bind_port_overrides_the_configured_port() {
        assert_eq!(
            resolve_listen("127.0.0.1", 8080, Some("*:9090"), None).unwrap(),
            (WILDCARD_HOST.to_string(), 9090)
        );
        // A bind without a port keeps the configured one.
        assert_eq!(
            resolve_listen("127.0.0.1", 8080, Some("localhost"), None).unwrap(),
            ("localhost".to_string(), 8080)
        );
        // -p applies on top of a host-only bind.
        assert_eq!(
            resolve_listen("127.0.0.1", 8080, Some("localhost"), Some(9090)).unwrap(),
            ("localhost".to_string(), 9090)
        );
    }

    #[test]
    fn a_bind_and_port_that_agree_are_allowed() {
        assert_eq!(
            resolve_listen("127.0.0.1", 8080, Some("*:9090"), Some(9090)).unwrap(),
            (WILDCARD_HOST.to_string(), 9090)
        );
    }

    #[test]
    fn a_bind_and_port_that_disagree_fail() {
        let error = resolve_listen("127.0.0.1", 8080, Some("*:9090"), Some(8080)).unwrap_err();
        assert!(error.contains("9090"), "{error}");
        assert!(error.contains("8080"), "{error}");
    }

    #[test]
    fn a_non_listening_host_or_wildcard_config_value_is_normalized() {
        assert_eq!(normalize_host("*"), WILDCARD_HOST);
        assert_eq!(normalize_host("  "), WILDCARD_HOST);
        assert_eq!(normalize_host("localhost"), "localhost");
    }

    #[test]
    fn a_config_bind_overrides_the_configured_host_and_port() {
        assert_eq!(
            resolve_listen_from_config("127.0.0.1", 8080, Some("*:9090"), None, None).unwrap(),
            (WILDCARD_HOST.to_string(), 9090)
        );
        // A config bind without a port keeps the configured one.
        assert_eq!(
            resolve_listen_from_config("127.0.0.1", 8080, Some("localhost"), None, None).unwrap(),
            ("localhost".to_string(), 8080)
        );
    }

    #[test]
    fn the_command_line_overrides_a_config_bind() {
        // -b wins over the config bind entirely.
        assert_eq!(
            resolve_listen_from_config(
                "127.0.0.1",
                8080,
                Some("*:9090"),
                Some("localhost:7070"),
                None
            )
            .unwrap(),
            ("localhost".to_string(), 7070)
        );
        // -p overrides the port a config bind carried.
        assert_eq!(
            resolve_listen_from_config("127.0.0.1", 8080, Some("*:9090"), None, Some(7070))
                .unwrap(),
            (WILDCARD_HOST.to_string(), 7070)
        );
        // -p applies on top of a host-only config bind too.
        assert_eq!(
            resolve_listen_from_config("127.0.0.1", 8080, Some("localhost"), None, Some(7070))
                .unwrap(),
            ("localhost".to_string(), 7070)
        );
    }

    #[test]
    fn an_invalid_config_bind_is_an_error() {
        let error =
            resolve_listen_from_config("127.0.0.1", 8080, Some("localhost:abc"), None, None)
                .unwrap_err();
        assert!(error.contains("config"), "{error}");
        assert!(error.contains("localhost:abc"), "{error}");
    }

    #[test]
    fn a_cli_bind_and_port_that_disagree_still_fail_with_a_config_bind() {
        let error = resolve_listen_from_config(
            "127.0.0.1",
            8080,
            Some("localhost"),
            Some("*:9090"),
            Some(8080),
        )
        .unwrap_err();
        assert!(error.contains("9090"), "{error}");
        assert!(error.contains("8080"), "{error}");
    }
}
