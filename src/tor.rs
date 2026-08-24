use crate::error::OnionError;
use reqwest::Client;
use std::time::Duration;

/// Strip the SOCKS scheme from a proxy address, leaving host:port.
pub fn strip_proxy_scheme(proxy_addr: &str) -> &str {
    proxy_addr
        .trim_start_matches("socks5h://")
        .trim_start_matches("socks5://")
}

/// Verify Tor actually routes traffic by requesting a known endpoint through
/// the proxy. Falls back to a plain TCP reachability check of the SOCKS port.
pub async fn check_tor_connection(proxy_addr: &str) -> bool {
    if let Ok(client) = build_client(proxy_addr) {
        let request = client.get("https://check.torproject.org/api/ip");
        if let Ok(Ok(response)) =
            tokio::time::timeout(Duration::from_secs(15), request.send()).await
        {
            if response.status().is_success() {
                return true;
            }
        }
    }

    tcp_reachable(strip_proxy_scheme(proxy_addr)).await
}

async fn tcp_reachable(addr: &str) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr),).await,
        Ok(Ok(_))
    )
}

/// Build a reqwest HTTP client configured to route through the Tor SOCKS5 proxy.
/// Uses `socks5h://` so that DNS resolution also happens over Tor.
///
/// No overall timeout is set — a single total timeout would abort large
/// downloads over slow circuits. Instead a per-read timeout keeps dead
/// connections from hanging forever while tolerating slow streams.
pub fn build_client(proxy_addr: &str) -> Result<Client, OnionError> {
    let proxy = reqwest::Proxy::all(proxy_addr)
        .map_err(|e| OnionError::TorUnavailable(format!("{}: {}", proxy_addr, e)))?;

    Client::builder()
        .proxy(proxy)
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| OnionError::TorUnavailable(e.to_string()))
}

/// Build a reqwest HTTP client for the normal network (same timeout policy).
pub fn build_normal_client() -> Result<Client, OnionError> {
    Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .build()
        .map_err(OnionError::Http)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_socks_schemes() {
        assert_eq!(
            strip_proxy_scheme("socks5h://127.0.0.1:9050"),
            "127.0.0.1:9050"
        );
        assert_eq!(
            strip_proxy_scheme("socks5://127.0.0.1:9150"),
            "127.0.0.1:9150"
        );
        assert_eq!(strip_proxy_scheme("127.0.0.1:9050"), "127.0.0.1:9050");
    }
}
