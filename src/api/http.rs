use anyhow::{Context, Result, anyhow};
use parking_lot::Mutex;
use serde_json::Value;
use std::io::Read;
use std::time::Duration;
use ureq::config::{ConfigBuilder, IpFamily};
use ureq::typestate::AgentScope;

pub const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct NetworkAgent(Mutex<Vec<CachedAgent>>);

struct CachedAgent {
    family: IpFamily,
    proxy: String,
    agent: ureq::Agent,
}

/// Set once YouTube's bot check was passed by switching address family (see
/// `switch_family`): new requests then use the other family for the rest of the session.
static SWITCHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The address family new requests use: the "Use IPv4 only" setting, turned
/// around while `switch_family` is in effect.
pub fn family(settings: &crate::store::Settings) -> IpFamily {
    match (settings.ipv4_only, SWITCHED.load(std::sync::atomic::Ordering::Relaxed)) {
        (false, false) => IpFamily::Any,
        (false, true) | (true, false) => IpFamily::Ipv4Only,
        (true, true) => IpFamily::Ipv6Only,
    }
}

/// YouTube's bot check is per address, and on some networks it flags the IPv6
/// address and not the IPv4 one, or the other way round. Turns the family new
/// requests use around (call again to undo); false when a proxy decides the route.
pub fn switch_family() -> bool {
    if crate::store::proxy_setting().is_some() {
        return false;
    }
    SWITCHED.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
    true
}

/// The yt-dlp flag for the family new requests use.
pub fn ytdlp_family_arg() -> Option<&'static str> {
    match family(&crate::store::settings_snapshot()) {
        IpFamily::Any => None,
        IpFamily::Ipv4Only => Some("--force-ipv4"),
        IpFamily::Ipv6Only => Some("--force-ipv6"),
    }
}

/// googlevideo URLs carry the address they were handed out to (`ip=` or
/// `/ip/<addr>/`) and only play from it: the family to fetch such a URL with.
pub fn family_of_url(url: &str) -> Option<IpFamily> {
    let ip = url
        .split(['?', '&'])
        .find_map(|p| p.strip_prefix("ip="))
        .or_else(|| url.split_once("/ip/").map(|(_, rest)| rest))?;
    let ip = ip.split(['/', '&', '?']).next()?;
    if ip.contains(':') || ip.to_ascii_lowercase().contains("%3a") {
        Some(IpFamily::Ipv6Only)
    } else if !ip.is_empty() && ip.chars().all(|c| c.is_ascii_digit() || c == '.') {
        Some(IpFamily::Ipv4Only)
    } else {
        None
    }
}

impl NetworkAgent {
    pub(crate) const fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    pub(crate) fn get(
        &self,
        settings: &crate::store::Settings,
        configure: impl FnOnce(ConfigBuilder<AgentScope>) -> ConfigBuilder<AgentScope>,
    ) -> ureq::Agent {
        self.get_with(settings, family(settings), configure)
    }

    /// An agent for the given address family (one is kept per family).
    pub(crate) fn get_with(
        &self,
        settings: &crate::store::Settings,
        family: IpFamily,
        configure: impl FnOnce(ConfigBuilder<AgentScope>) -> ConfigBuilder<AgentScope>,
    ) -> ureq::Agent {
        let mut cached = self.0.lock();
        cached.retain(|c| c.proxy == settings.proxy);
        if let Some(current) = cached.iter().find(|c| c.family == family) {
            return current.agent.clone();
        }
        let mut builder = configure(ureq::Agent::config_builder()).ip_family(family);
        if !settings.proxy.trim().is_empty() {
            if let Ok(proxy) = ureq::Proxy::new(&settings.proxy) {
                builder = builder.proxy(Some(proxy));
            }
        }
        let agent = builder.build().new_agent();
        cached.push(CachedAgent {
            family,
            proxy: settings.proxy.clone(),
            agent: agent.clone(),
        });
        agent
    }
}

fn agent() -> ureq::Agent {
    static AGENT: NetworkAgent = NetworkAgent::new();
    AGENT.get(&crate::store::settings_snapshot(), |builder| {
        builder
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .user_agent(BROWSER_UA)
    })
}

#[cfg(test)]
mod tests {
    use super::{IpFamily, NetworkAgent};
    use crate::store::Settings;

    #[test]
    fn changes_address_family_after_the_first_request() {
        let cache = NetworkAgent::new();
        let mut settings = Settings {
            ipv4_only: true,
            ..Settings::default()
        };
        let initial = cache.get(&settings, |builder| builder);
        assert_eq!(initial.config().ip_family(), IpFamily::Ipv4Only);

        settings.ipv4_only = false;
        let updated = cache.get(&settings, |builder| builder);
        assert_eq!(updated.config().ip_family(), IpFamily::Any);
        assert_eq!(initial.config().ip_family(), IpFamily::Ipv4Only);

        settings.ipv4_only = true;
        let restored = cache.get(&settings, |builder| builder);
        assert_eq!(restored.config().ip_family(), IpFamily::Ipv4Only);
    }

    #[test]
    fn reads_the_address_family_a_stream_url_is_bound_to() {
        use super::family_of_url;
        assert_eq!(family_of_url("https://r1.googlevideo.com/videoplayback?expire=1&ip=90.231.8.221&id=x"), Some(IpFamily::Ipv4Only));
        assert_eq!(family_of_url("https://r1.googlevideo.com/videoplayback?ip=2a02%3A1234%3A%3A1&id=x"), Some(IpFamily::Ipv6Only));
        assert_eq!(family_of_url("https://manifest.googlevideo.com/api/manifest/hls_variant/expire/1/ip/2a02:1234::1/id/x/file/index.m3u8"), Some(IpFamily::Ipv6Only));
        assert_eq!(family_of_url("https://manifest.googlevideo.com/api/manifest/hls_playlist/ip/90.231.8.221/itag/96/index.m3u8"), Some(IpFamily::Ipv4Only));
        assert_eq!(family_of_url("https://www.youtube.com/api/timedtext?v=x&lang=en"), None);
        assert_eq!(family_of_url("https://example.com/?skip=1"), None);
    }

    #[test]
    #[ignore = "streams a local response for over 30 seconds"]
    fn slow_download_can_keep_making_progress() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::{Ipv4Addr, TcpListener};
        use std::time::Duration;

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let url = format!("http://{}/download", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut request = BufReader::new(&mut stream);
            let mut line = String::new();
            loop {
                line.clear();
                assert!(request.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\ns").unwrap();
            for byte in b"low!" {
                std::thread::sleep(Duration::from_secs(8));
                stream.write_all(&[*byte]).unwrap();
            }
        });
        let mut bytes = Vec::new();
        let mut progress = 0;
        super::download_to(&url, &mut bytes, |done, total| {
            assert_eq!(total, Some(5));
            assert!(done > progress);
            progress = done;
            true
        }).unwrap();
        server.join().unwrap();
        assert_eq!(bytes, b"slow!");
        assert_eq!(progress, 5);
    }

    #[test]
    fn tracking_requests_do_not_follow_redirects() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let redirect = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = destination.local_addr().unwrap();
        let url = format!("http://{}/stats", redirect.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = redirect.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            for line in BufReader::new(&stream).lines() {
                if line.unwrap().is_empty() {
                    break;
                }
            }
            write!(stream, "HTTP/1.1 302 Found\r\nLocation: http://{target}/other\r\nContent-Length: 0\r\n\r\n").unwrap();
        });
        let status = super::tracking_status(&url, &[("Cookie", "test=synthetic")]).unwrap();
        assert_eq!(status, 302);
        server.join().unwrap();
        assert_eq!(destination.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }
}

pub fn get_bytes(url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
    let mut req = agent().get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req.call().with_context(|| format!("GET {url}"))?;
    let status = resp.status().as_u16();
    let mut buf = Vec::new();
    resp.body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .reader()
        .read_to_end(&mut buf)?;
    if status >= 400 {
        return Err(anyhow!("HTTP {status} for {url}"));
    }
    Ok(buf)
}

pub fn get_text(url: &str) -> Result<String> {
    Ok(String::from_utf8_lossy(&get_bytes(url, &[])?).into_owned())
}

/// Playback tracking URLs carry account data. Do not forward their headers to
/// redirects, and keep the reporter's network wait bounded.
pub(super) fn tracking_status(url: &str, headers: &[(&str, &str)]) -> std::result::Result<u16, ureq::Error> {
    let mut req = agent().get(url)
        .config()
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(2)))
        .build();
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    req.call().map(|response| response.status().as_u16())
}

pub fn get_json(url: &str) -> Result<Value> {
    let b = get_bytes(url, &[("Accept", "application/json")])?;
    serde_json::from_slice(&b).context("invalid json")
}

/// Returns (status, json)
pub fn get_json_status(url: &str) -> Result<(u16, Value)> {
    let mut resp = agent().get(url).header("Accept", "application/json").call()?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

pub fn post_json(url: &str, body: &Value, headers: &[(&str, &str)]) -> Result<Value> {
    let mut req = agent().post(url).header("Content-Type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req
        .send(serde_json::to_vec(body)?)
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_string()?;
    if status >= 400 {
        return Err(anyhow!("HTTP {status}: {}", text.chars().take(300).collect::<String>()));
    }
    serde_json::from_str(&text).context("invalid json")
}

/// POST returning the status code and JSON body even for error statuses.
pub fn post_json_status(url: &str, body: &Value, headers: &[(&str, &str)]) -> Result<(u16, Value)> {
    send_json_status(agent().post(url), body, headers)
}

pub(super) fn post_session_json(url: &str, body: &Value, headers: &[(&str, &str)]) -> Result<(u16, Value)> {
    let request = agent().post(url).config().max_redirects(0).build();
    send_json_status(request, body, headers)
}

fn send_json_status(
    request: ureq::RequestBuilder<ureq::typestate::WithBody>,
    body: &Value,
    headers: &[(&str, &str)],
) -> Result<(u16, Value)> {
    let mut req = request.header("Content-Type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut resp = req.send(serde_json::to_vec(body)?)?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().with_config().limit(64 * 1024 * 1024).read_to_string()?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

/// Streaming download to a writer with progress callback (downloaded, total).
pub fn download_to(
    url: &str,
    out: &mut dyn std::io::Write,
    mut progress: impl FnMut(u64, Option<u64>) -> bool,
) -> Result<()> {
    // A healthy transfer can take longer than a small API request.
    let mut resp = agent()
        .get(url)
        .config()
        .timeout_global(None)
        .timeout_resolve(Some(REQUEST_TIMEOUT))
        .timeout_connect(Some(REQUEST_TIMEOUT))
        .timeout_send_request(Some(REQUEST_TIMEOUT))
        .timeout_recv_response(Some(REQUEST_TIMEOUT))
        .timeout_per_read(Some(REQUEST_TIMEOUT))
        .build()
        .call()?;
    if resp.status().as_u16() >= 400 {
        return Err(anyhow!("HTTP {}", resp.status()));
    }
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let mut reader = resp.body_mut().with_config().limit(u64::MAX).reader();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
        if !progress(done, total) {
            return Err(anyhow!("cancelled"));
        }
    }
    Ok(())
}

pub fn urlencode(s: &str) -> String {
    let mut o = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

/// Decodes %XX escapes and '+' (query strings).
pub fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
