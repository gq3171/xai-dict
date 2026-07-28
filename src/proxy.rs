use anyhow::{Context, Result};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Resolve an HTTP proxy URL for outbound HTTPS (STT).
///
/// Order:
/// 1. `config_proxy` (from config.toml / CLI)
/// 2. `HTTPS_PROXY` / `https_proxy` / `HTTP_PROXY` / `http_proxy` / `ALL_PROXY` / `all_proxy`
/// 3. Probe common local Clash/Mihomo/V2Ray ports and use the first that accepts TCP
pub fn resolve_http_proxy(config_proxy: Option<&str>) -> Option<String> {
    if let Some(p) = config_proxy.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(normalize_proxy(p));
    }

    for var in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                // socks5 works for some clients; reqwest needs socks feature for socks://
                // Prefer http:// form when possible.
                return Some(normalize_proxy(v));
            }
        }
    }

    // Common local proxy ports (Clash / Mihomo / Verge / V2RayN)
    for port in [7897u16, 7890, 7891, 10809, 20171, 1087, 8080, 8888] {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        if tcp_open(addr, Duration::from_millis(80)) {
            let url = format!("http://127.0.0.1:{port}");
            tracing::info!(%url, "auto-detected local HTTP proxy");
            return Some(url);
        }
    }

    None
}

fn normalize_proxy(raw: &str) -> String {
    let raw = raw.trim();
    // Convert socks5://host:port to http:// only when it's the all_proxy style
    // and an http proxy is also likely; keep socks if that's all we have — reqwest
    // needs the `socks` feature for socks5. Prefer http:// for local Clash mixed port.
    if let Some(rest) = raw
        .strip_prefix("socks5://")
        .or_else(|| raw.strip_prefix("socks5h://"))
        .or_else(|| raw.strip_prefix("socks://"))
    {
        // Clash mixed port usually speaks HTTP CONNECT on the same port.
        return format!("http://{rest}");
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return raw.to_string();
    }
    // bare host:port
    format!("http://{raw}")
}

fn tcp_open(addr: SocketAddr, timeout: Duration) -> bool {
    TcpStream::connect_timeout(&addr, timeout).is_ok()
}

pub fn build_client(proxy_url: Option<&str>) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(20));

    if let Some(url) = proxy_url {
        let proxy = reqwest::Proxy::all(url)
            .with_context(|| format!("invalid proxy URL: {url}"))?;
        b = b.proxy(proxy);
        tracing::info!(proxy = %url, "HTTP client using proxy");
    } else {
        // Explicitly avoid silent system-proxy surprises; we already scanned env.
        b = b.no_proxy();
        tracing::warn!(
            "no proxy configured — direct connect to api.x.ai often times out in China; \
             set HTTPS_PROXY or [proxy] in ~/.config/xai-dict/config.toml"
        );
    }

    b.build().context("build HTTP client")
}

/// Best-effort DNS note for diagnostics.
pub fn diagnose_api_host(host: &str) -> String {
    match (host, 443).to_socket_addrs() {
        Ok(addrs) => {
            let list: Vec<_> = addrs.take(4).map(|a| a.to_string()).collect();
            format!("{host} -> {}", list.join(", "))
        }
        Err(e) => format!("{host} resolve failed: {e}"),
    }
}
