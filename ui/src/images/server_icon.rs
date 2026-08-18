//! The MSSP `ICON` pipeline: fetch, transcode, and cache the square icon a
//! game advertises, for display beside the server name on the connect screen.
//!
//! An `ICON` value is an attacker-controlled URL, and fetching it is an
//! outbound HTTP request that reveals the user's IP address to whichever host
//! the game names. The posture mirrors the link-trust philosophy:
//!
//! - **https only**, redirects capped with [`icon_url_policy`] re-applied per
//!   hop, the spec's own 256KB transfer cap enforced while streaming, explicit
//!   timeouts, and a dedicated client with no cookies or default headers.
//! - **Private/link-local ranges are refused after resolution**, and the
//!   vetted addresses are pinned for the connection so a rebinding DNS answer
//!   cannot redirect it ([`vet_addresses`]).
//! - **Auto-fetch only when no new information flows**: the icon URL's host is
//!   the game's own host or a parent domain of it (connecting already reveals
//!   our IP there), or a host covered by the server's link-trust grants
//!   (`trusted_link_hosts` / `trust_all_links` — icons and links share one
//!   trust store). Anything else holds at [`IconUrlPolicy::NeedsConsent`]
//!   until the user grants the host through the link flow.
//! - **The cached artifact is our bytes, not theirs**: strict decode with
//!   dimension caps, downscale to [`CACHED_DIMENSION`], re-encode PNG into
//!   the server's directory (`icon.png`, plus `icon.src` recording the `ICON`
//!   value it came from — a refetch happens only when that value changes).

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use image::ImageEncoder;
use smudgy_core::models::server::ServerConfig;
use url::{Host, Url};

/// Transfer cap for an encoded icon — the MSSP spec's own limit for `ICON`.
pub const MAX_ICON_BYTES: u64 = 256 * 1024;

/// Decode-input sanity cap for cache reads. Looser than the transfer cap: this
/// module's own re-encode of an incompressible 256KB source can exceed 256KB
/// by the PNG container overhead, and refusing to reload it would orphan the
/// artifact just written.
const MAX_DECODE_INPUT_BYTES: usize = 1024 * 1024;

/// Decode-time pixel bounds: a 256KB file can decompress enormously, so the
/// dimension caps and the transient-allocation cap are what bound memory.
const MAX_SOURCE_DIMENSION: u32 = 2048;
const MAX_DECODE_ALLOC_BYTES: u64 = 64 * 1024 * 1024;

/// Longest edge of the cached artifact. Display is ~28px; the headroom keeps
/// the cache useful for hi-DPI and future larger placements.
const CACHED_DIMENSION: u32 = 256;

/// Max redirect hops before giving up.
const MAX_REDIRECTS: usize = 5;

/// The verdict on one `ICON` URL, decided before any network I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconUrlPolicy {
    /// Fetch without asking: https, and the host is the game's own host, a
    /// parent domain of it, or already covered by the link-trust grants.
    AutoFetch(Url),
    /// A well-formed https URL on an unrelated host: fetching would leak the
    /// user's IP somewhere connecting doesn't, so it waits for the same
    /// per-host grant a server link needs.
    NeedsConsent { host: String },
    /// Never fetched: not https, or no usable host.
    Refused(String),
}

/// Decide what to do with an `ICON` value for a server dialed at `game_host`.
/// Pure and lexical — the private-range refusal happens at fetch time, after
/// resolution. Applied to every redirect hop as well: a same-host redirect
/// bouncing to an unvetted third party gets the same verdict the original URL
/// would have.
#[must_use]
pub fn icon_url_policy(raw: &str, game_host: &str, config: &ServerConfig) -> IconUrlPolicy {
    let Ok(url) = Url::parse(raw.trim()) else {
        return IconUrlPolicy::Refused("ICON is not a URL".to_string());
    };
    if url.scheme() != "https" {
        return IconUrlPolicy::Refused(format!(
            "{}: icons are fetched over https only",
            url.scheme()
        ));
    }
    // The host comes from the parsed URL, never the raw string —
    // `https://ok.com@evil.com/x` must resolve to `evil.com`. The parsed forms
    // (IDNA-lowercased domains, canonical IP spellings) match the trust
    // store's canonical grant keys.
    let host = match url.host() {
        Some(Host::Domain(domain)) => domain.to_string(),
        Some(Host::Ipv4(address)) => address.to_string(),
        Some(Host::Ipv6(address)) => address.to_string(),
        None => return IconUrlPolicy::Refused("ICON URL has no host".to_string()),
    };
    if host_is_game_or_parent(&host, game_host) || config.allows_server_link(Some(&host)) {
        IconUrlPolicy::AutoFetch(url)
    } else {
        IconUrlPolicy::NeedsConsent { host }
    }
}

/// Whether `icon_host` is the dialed game host or a parent domain of it —
/// the "no new information flows" test. Parent-domain containment applies to
/// domain names only; IP literals must match exactly.
fn host_is_game_or_parent(icon_host: &str, game_host: &str) -> bool {
    let Some(game) = canonical_host(game_host) else {
        return false;
    };
    if icon_host.eq_ignore_ascii_case(&game) {
        return true;
    }
    if icon_host.parse::<IpAddr>().is_ok() || game.parse::<IpAddr>().is_ok() {
        return false;
    }
    game.to_ascii_lowercase()
        .ends_with(&format!(".{}", icon_host.to_ascii_lowercase()))
}

/// Canonical form of a configured host (IDNA-lowercased domain, canonical IP
/// spelling — bare IPv6 accepted, as `server.json` stores it unbracketed).
fn canonical_host(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if let Ok(address) = raw.parse::<std::net::Ipv6Addr>() {
        return Some(address.to_string());
    }
    match Host::parse(raw).ok()? {
        Host::Domain(domain) => Some(domain),
        Host::Ipv4(address) => Some(address.to_string()),
        Host::Ipv6(address) => Some(address.to_string()),
    }
}

/// Whether an address is publicly routable — the post-resolution gate. A game
/// must not be able to aim the icon fetch at loopback, RFC 1918/4193 space,
/// link-local (cloud metadata endpoints live there), CGNAT, or other special
/// ranges.
fn ip_is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                // CGNAT 100.64.0.0/10.
                || (octets[0] == 100 && (octets[1] & 0b1100_0000) == 64)
                // IETF protocol assignments 192.0.0.0/24.
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                // Benchmarking 198.18.0.0/15.
                || (octets[0] == 198 && (octets[1] & 0b1111_1110) == 18)
                // Reserved 240.0.0.0/4 (255.255.255.255 already covered).
                || octets[0] >= 240)
        }
        IpAddr::V6(v6) => {
            // A v4-mapped address is the v4 address; judge it as one so
            // `::ffff:192.168.1.1` cannot slip past the v4 ranges. The other
            // v4-embedding forms below get the same treatment: each names an
            // IPv4 target the network may literally route to (NAT64 gateways
            // translate, 6to4/Teredo tunnel), so each is judged as that
            // target.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return ip_is_public(IpAddr::V4(mapped));
            }
            let segments = v6.segments();
            let embedded_v4 = |hi: u16, lo: u16| {
                let [a, b] = hi.to_be_bytes();
                let [c, d] = lo.to_be_bytes();
                IpAddr::V4(std::net::Ipv4Addr::new(a, b, c, d))
            };
            // Deprecated IPv4-compatible ::a.b.c.d (`::` and `::1` are the
            // unspecified/loopback checks' to judge, below).
            if segments[..6] == [0, 0, 0, 0, 0, 0] && !(v6.is_unspecified() || v6.is_loopback()) {
                return ip_is_public(embedded_v4(segments[6], segments[7]));
            }
            // NAT64 well-known prefix 64:ff9b::/96.
            if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return ip_is_public(embedded_v4(segments[6], segments[7]));
            }
            // 6to4 2002::/16 embeds the v4 in the next 32 bits.
            if segments[0] == 0x2002 {
                return ip_is_public(embedded_v4(segments[1], segments[2]));
            }
            // Teredo 2001::/32 embeds the client v4 bit-inverted in the tail.
            if segments[0] == 0x2001 && segments[1] == 0 {
                return ip_is_public(embedded_v4(!segments[6], !segments[7]));
            }
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                // Unique local fc00::/7.
                || (segments[0] & 0xfe00) == 0xfc00
                // Link-local fe80::/10.
                || (segments[0] & 0xffc0) == 0xfe80
                // Documentation 2001:db8::/32.
                || (segments[0] == 0x2001 && segments[1] == 0xdb8))
        }
    }
}

/// Resolve a URL's host and refuse any non-public answer — fail-closed: one
/// private address among the results rejects the whole fetch (a split answer
/// is exactly what a rebinding resolver serves). For a domain, returns the
/// vetted addresses so the connection can be pinned to them; an IP-literal
/// host needs no pinning.
async fn vet_addresses(url: &Url) -> Result<Option<(String, Vec<SocketAddr>)>, String> {
    let port = url.port_or_known_default().unwrap_or(443);
    match url.host() {
        Some(Host::Domain(domain)) => {
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host((domain, port))
                .await
                .map_err(|e| format!("{domain}: DNS lookup failed: {e}"))?
                .collect();
            if addresses.is_empty() {
                return Err(format!("{domain}: no addresses"));
            }
            if let Some(bad) = addresses.iter().find(|address| !ip_is_public(address.ip())) {
                return Err(format!(
                    "{domain} resolves to the non-public address {}",
                    bad.ip()
                ));
            }
            Ok(Some((domain.to_string(), addresses)))
        }
        Some(Host::Ipv4(address)) if ip_is_public(IpAddr::V4(address)) => Ok(None),
        Some(Host::Ipv6(address)) if ip_is_public(IpAddr::V6(address)) => Ok(None),
        Some(_) => Err(format!(
            "{} is not a public address",
            url.host_str().unwrap_or("?")
        )),
        None => Err("no host".to_string()),
    }
}

/// Fetch the icon bytes with the full posture: per-hop policy, per-hop address
/// vetting with the vetted answers pinned, streaming size cap, timeouts, no
/// cookies, no default headers beyond a bare product token (UA-less requests
/// trip WAF/CDN bot rules).
async fn fetch_icon_bytes(
    start: Url,
    game_host: &str,
    config: &ServerConfig,
) -> Result<Vec<u8>, String> {
    let mut current = start;
    for _hop in 0..=MAX_REDIRECTS {
        current = match icon_url_policy(current.as_str(), game_host, config) {
            IconUrlPolicy::AutoFetch(url) => url,
            IconUrlPolicy::NeedsConsent { host } => {
                return Err(format!(
                    "{host} is neither the game's host nor a granted link host"
                ));
            }
            IconUrlPolicy::Refused(reason) => return Err(reason),
        };
        let pinned = vet_addresses(&current).await?;
        let mut builder = reqwest::Client::builder()
            .user_agent("smudgy")
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30));
        if let Some((domain, addresses)) = &pinned {
            builder = builder.resolve_to_addrs(domain, addresses);
        }
        let client = builder.build().map_err(|e| format!("http client: {e}"))?;
        let response = client
            .get(current.clone())
            .send()
            .await
            .map_err(|e| format!("{current}: {e}"))?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| format!("{current}: redirect without a Location"))?;
            current = current
                .join(location)
                .map_err(|e| format!("bad redirect target {location}: {e}"))?;
            continue;
        }
        if !status.is_success() {
            return Err(format!("{current}: HTTP {status}"));
        }
        // Enforce the cap up front when the server declares a length, and
        // while streaming regardless — a chunked response has no
        // Content-Length, and buffering it whole first would hand a hostile
        // server an OOM lever.
        if let Some(length) = response.content_length()
            && length > MAX_ICON_BYTES
        {
            return Err(format!(
                "{current}: {length} bytes exceeds the {MAX_ICON_BYTES} cap"
            ));
        }
        let mut response = response;
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("{current}: {e}"))?
        {
            if (body.len() + chunk.len()) as u64 > MAX_ICON_BYTES {
                return Err(format!("{current}: body exceeds the {MAX_ICON_BYTES} cap"));
            }
            body.extend_from_slice(&chunk);
        }
        return Ok(body);
    }
    Err("too many redirects".to_string())
}

/// Decode with icon-scale bounds to straight RGBA8. Shared by the fetch path
/// and cache reads — the cache normally holds this module's own small
/// re-encode, but the file is user-writable and gets the same distrust.
fn decode_bounded_rgba(bytes: &[u8]) -> Result<image::RgbaImage, String> {
    if bytes.len() > MAX_DECODE_INPUT_BYTES {
        return Err(format!(
            "icon is {} bytes; capped at {MAX_DECODE_INPUT_BYTES}",
            bytes.len()
        ));
    }
    if super::looks_like_svg(bytes) {
        return Err("SVG icons are not supported (raster images only)".to_string());
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("unrecognized image data: {e}"))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|e| format!("decode: {e}"))?;
    let rgba = decoded.into_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return Err("empty image".to_string());
    }
    Ok(rgba)
}

/// The re-encoded cache artifact plus the decoded pixels it was encoded from
/// (so the caller can build a display handle without a second decode).
struct IconArtifact {
    png: Vec<u8>,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// Strict decode → downscale → PNG re-encode: the cached artifact is our
/// bytes, not the server's.
fn transcode_icon(bytes: &[u8]) -> Result<IconArtifact, String> {
    if bytes.len() as u64 > MAX_ICON_BYTES {
        return Err(format!(
            "icon is {} bytes; the spec caps ICON at {MAX_ICON_BYTES}",
            bytes.len()
        ));
    }
    let mut rgba = decode_bounded_rgba(bytes)?;
    let (width, height) = rgba.dimensions();
    let longest = width.max(height);
    if longest > CACHED_DIMENSION {
        let scale = f64::from(CACHED_DIMENSION) / f64::from(longest);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let scaled_width = ((f64::from(width) * scale).round() as u32).max(1);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let scaled_height = ((f64::from(height) * scale).round() as u32).max(1);
        rgba = image::imageops::thumbnail(&rgba, scaled_width, scaled_height);
    }
    let (width, height) = rgba.dimensions();
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            rgba.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| format!("png encode: {e}"))?;
    Ok(IconArtifact {
        png,
        width,
        height,
        rgba: rgba.into_raw(),
    })
}

/// The cache file pair inside the server's own directory: the PNG artifact
/// and the `ICON` value it was produced from.
fn icon_paths(server: &str) -> Option<(PathBuf, PathBuf)> {
    let dir = smudgy_core::get_smudgy_home().ok()?.join(server);
    Some((dir.join("icon.png"), dir.join("icon.src")))
}

/// The `ICON` value the cached artifact was fetched from, if any.
fn cached_icon_source(server: &str) -> Option<String> {
    let (_, src_path) = icon_paths(server)?;
    std::fs::read_to_string(src_path)
        .ok()
        .map(|value| value.trim().to_string())
}

/// Whether `icon_value` differs from what the cache was built from — the
/// only condition under which a fetch is attempted.
#[must_use]
pub fn needs_refetch(server: &str, icon_value: &str) -> bool {
    cached_icon_source(server).as_deref() != Some(icon_value.trim())
}

/// Load the cached artifact as a display handle. `None` when there is no
/// cache or it doesn't decode — the caller falls back to iconless rendering.
/// Pre-decoded to RGBA because iced is built without codecs.
#[must_use]
pub fn load_cached_icon(server: &str) -> Option<iced::widget::image::Handle> {
    let (png_path, _) = icon_paths(server)?;
    let bytes = std::fs::read(png_path).ok()?;
    let rgba = decode_bounded_rgba(&bytes).ok()?;
    let (width, height) = rgba.dimensions();
    Some(iced::widget::image::Handle::from_rgba(
        width,
        height,
        rgba.into_raw(),
    ))
}

/// Fetch `icon_value` under the full posture, transcode, persist into the
/// server directory, and return the display handle. Failures are logged and
/// yield `None` — any previously cached icon keeps rendering, and the
/// unchanged `icon.src` means the next reconcile simply tries again.
pub async fn fetch_and_cache(
    server: String,
    game_host: String,
    config: ServerConfig,
    icon_value: String,
) -> (String, Option<iced::widget::image::Handle>) {
    let url = match icon_url_policy(&icon_value, &game_host, &config) {
        IconUrlPolicy::AutoFetch(url) => url,
        IconUrlPolicy::NeedsConsent { host } => {
            log::info!("smudgy icons: {server}: {host} held for a link-trust grant");
            return (server, None);
        }
        IconUrlPolicy::Refused(reason) => {
            log::info!("smudgy icons: {server}: {reason}");
            return (server, None);
        }
    };
    let artifact = match fetch_icon_bytes(url, &game_host, &config).await {
        Ok(body) => match transcode_icon(&body) {
            Ok(artifact) => artifact,
            Err(reason) => {
                log::warn!("smudgy icons: {server}: {reason}");
                return (server, None);
            }
        },
        Err(reason) => {
            log::warn!("smudgy icons: {server}: {reason}");
            return (server, None);
        }
    };
    let Some((png_path, src_path)) = icon_paths(&server) else {
        return (server, None);
    };
    // Artifact first, source marker second: a failure between the two
    // refetches next time rather than pinning a missing artifact.
    if let Err(e) = smudgy_core::models::persistence::write_atomic(&png_path, &artifact.png) {
        log::warn!("smudgy icons: failed to write {}: {e}", png_path.display());
        return (server, None);
    }
    if let Err(e) =
        smudgy_core::models::persistence::write_atomic(&src_path, icon_value.trim().as_bytes())
    {
        log::warn!("smudgy icons: failed to write {}: {e}", src_path.display());
    }
    let handle =
        iced::widget::image::Handle::from_rgba(artifact.width, artifact.height, artifact.rgba);
    (server, Some(handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(trusted: &[&str], trust_all: bool) -> ServerConfig {
        ServerConfig {
            trusted_link_hosts: trusted.iter().map(ToString::to_string).collect(),
            trust_all_links: trust_all,
            ..ServerConfig::new("arctic.example.com".to_string(), 2700)
        }
    }

    fn policy(url: &str, game_host: &str, config: &ServerConfig) -> IconUrlPolicy {
        icon_url_policy(url, game_host, config)
    }

    #[test]
    fn https_is_the_only_scheme() {
        let c = config(&[], false);
        for url in [
            "http://arctic.example.com/icon.png",
            "ftp://arctic.example.com/icon.png",
            "file:///etc/passwd",
            "data:image/png;base64,xxxx",
        ] {
            assert!(
                matches!(
                    policy(url, "arctic.example.com", &c),
                    IconUrlPolicy::Refused(_)
                ),
                "{url} must be refused"
            );
        }
        assert!(matches!(
            policy(
                "https://arctic.example.com/icon.png",
                "arctic.example.com",
                &c
            ),
            IconUrlPolicy::AutoFetch(_)
        ));
    }

    #[test]
    fn garbage_is_refused_not_consented() {
        let c = config(&[], false);
        assert!(matches!(
            policy("not a url at all", "arctic.example.com", &c),
            IconUrlPolicy::Refused(_)
        ));
    }

    #[test]
    fn the_games_own_host_and_parents_auto_fetch() {
        let c = config(&[], false);
        // Exact host, case-insensitively.
        assert!(matches!(
            policy("https://Arctic.Example.COM/i.png", "arctic.example.com", &c),
            IconUrlPolicy::AutoFetch(_)
        ));
        // A parent domain of the dialed host: no new information flows.
        assert!(matches!(
            policy("https://example.com/i.png", "arctic.example.com", &c),
            IconUrlPolicy::AutoFetch(_)
        ));
        // A sibling is NOT a parent.
        assert!(matches!(
            policy("https://cdn.example.com/i.png", "arctic.example.com", &c),
            IconUrlPolicy::NeedsConsent { .. }
        ));
        // Suffix without a label boundary is not containment.
        assert!(matches!(
            policy("https://ple.com/i.png", "arctic.example.com", &c),
            IconUrlPolicy::NeedsConsent { .. }
        ));
    }

    #[test]
    fn unrelated_hosts_hold_for_consent_unless_granted() {
        let ungranted = config(&[], false);
        assert!(matches!(
            policy("https://imgur.com/i.png", "arctic.example.com", &ungranted),
            IconUrlPolicy::NeedsConsent { ref host } if host == "imgur.com"
        ));
        // A per-host grant in the shared trust store unlocks it.
        let granted = config(&["imgur.com"], false);
        assert!(matches!(
            policy("https://imgur.com/i.png", "arctic.example.com", &granted),
            IconUrlPolicy::AutoFetch(_)
        ));
        // So does the blanket grant.
        let blanket = config(&[], true);
        assert!(matches!(
            policy("https://imgur.com/i.png", "arctic.example.com", &blanket),
            IconUrlPolicy::AutoFetch(_)
        ));
    }

    #[test]
    fn the_host_comes_from_the_parsed_url_never_the_raw_string() {
        let c = config(&[], false);
        // Userinfo trick: the real host is evil.com.
        assert!(matches!(
            policy(
                "https://arctic.example.com@evil.com/i.png",
                "arctic.example.com",
                &c
            ),
            IconUrlPolicy::NeedsConsent { ref host } if host == "evil.com"
        ));
    }

    #[test]
    fn ip_literal_hosts_match_exactly_and_never_by_parentage() {
        let c = config(&[], false);
        assert!(matches!(
            policy("https://203.0.113.7/i.png", "203.0.113.7", &c),
            IconUrlPolicy::AutoFetch(_)
        ));
        assert!(matches!(
            policy("https://203.0.113.8/i.png", "203.0.113.7", &c),
            IconUrlPolicy::NeedsConsent { .. }
        ));
        // A domain icon host is never a "parent" of an IP-dialed game.
        assert!(matches!(
            policy("https://example.com/i.png", "203.0.113.7", &c),
            IconUrlPolicy::NeedsConsent { .. }
        ));
    }

    #[test]
    fn private_and_special_ranges_are_not_public() {
        for bad in [
            "0.0.0.0",
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "192.0.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "2001:db8::1",
            "ff02::1",
            "::ffff:192.168.1.1",
        ] {
            assert!(!ip_is_public(bad.parse().unwrap()), "{bad} must be refused");
        }
        // The three TEST-NET documentation blocks are refused too.
        for bad in ["192.0.2.1", "198.51.100.1", "203.0.113.7"] {
            assert!(!ip_is_public(bad.parse().unwrap()), "{bad} must be refused");
        }
        for good in ["1.1.1.1", "8.8.8.8", "2607:f8b0::1", "::ffff:8.8.8.8"] {
            assert!(
                ip_is_public(good.parse().unwrap()),
                "{good} must be allowed"
            );
        }
    }

    #[test]
    fn v4_embedding_transition_forms_are_judged_as_their_target() {
        // Each of these routes (via NAT64 translation or 6to4/Teredo
        // tunneling, or the deprecated v4-compatible form) to the IPv4
        // address it embeds: a private target is refused however it is
        // spelled. Teredo's client address is stored bit-inverted.
        for bad in [
            "64:ff9b::10.0.0.1",                    // NAT64 → 10.0.0.1
            "2002:a00:1::",                         // 6to4 → 10.0.0.1
            "::10.0.0.1",                           // v4-compatible → 10.0.0.1
            "2001:0:4136:e378:8000:63bf:f5f5:fffd", // Teredo → 10.10.0.2
            "198.18.0.1",                           // benchmarking 198.18.0.0/15
        ] {
            assert!(!ip_is_public(bad.parse().unwrap()), "{bad} must be refused");
        }
        // A public embedded target stays public.
        for good in ["64:ff9b::8.8.8.8", "2002:808:808::"] {
            assert!(
                ip_is_public(good.parse().unwrap()),
                "{good} must be allowed"
            );
        }
    }

    fn encoded_png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(width, height, image::Rgba([0, 128, 255, 255]));
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(img.as_raw(), width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        out
    }

    #[test]
    fn transcode_round_trips_small_icons_untouched() {
        let artifact = transcode_icon(&encoded_png(64, 64)).unwrap();
        assert_eq!((artifact.width, artifact.height), (64, 64));
        // The artifact decodes back with the shared bounded decoder.
        let rgba = decode_bounded_rgba(&artifact.png).unwrap();
        assert_eq!(rgba.dimensions(), (64, 64));
        assert_eq!(rgba.as_raw(), &artifact.rgba);
    }

    #[test]
    fn transcode_downscales_oversized_icons_preserving_aspect() {
        let artifact = transcode_icon(&encoded_png(600, 300)).unwrap();
        assert_eq!((artifact.width, artifact.height), (256, 128));
    }

    #[test]
    fn transcode_rejects_svg_garbage_and_oversize() {
        assert!(transcode_icon(b"  <svg xmlns='x'></svg>").is_err());
        assert!(transcode_icon(&[0u8; 64]).is_err());
        let oversized = vec![0u8; (MAX_ICON_BYTES + 1) as usize];
        assert!(transcode_icon(&oversized).is_err());
    }
}
