//! The observed-metadata band: what the server itself said last time we were
//! there (`observed.json` — MSSP variables plus connection timestamps),
//! rendered on the server detail pane. Everything here is optional and
//! attacker-controlled display data: numeric interpretation happens at this
//! display edge only, values that don't parse render as absent, and text is
//! escaped/elided before it reaches a widget. A server without a sidecar
//! renders exactly as it did before the feature existed.

use chrono::{DateTime, Utc};
use iced::widget::{
    Column, Row, button, center, checkbox, container, mouse_area, opaque, row, space, text,
};
use iced::{Alignment, Border, Color, Length, Padding};

use smudgy_core::models::observed::{ObservedServer, ObservedValue};
use smudgy_core::models::server::{Server, link_url_host};
use smudgy_core::session::styled_line::{InvisiblePolicy, escape_invisible_text};

use crate::i18n::t;
use crate::theme::Element;
use crate::theme::builtins;

use super::{Message, ObservedLinkConfirm, State};

/// `UPTIME` values before this instant (1990-01-01T00:00:00Z) predate the
/// genre and render as absent, like values in the future — the spec's uptime
/// is a unix start time, and servers lie or misformat.
const UPTIME_EPOCH_FLOOR: i64 = 631_152_000;

/// Player counts outside this range render as absent (a lie either way).
const MAX_PLAYERS: i64 = 1_000_000;

const LINK_CONFIRM_DISPLAY_CHARS: usize = 180;

// --- Display-edge interpretation (pure; storage stays strings) ---

/// The scalar text of one variable; an array-form value has no scalar
/// interpretation here and reads as absent.
fn scalar<'a>(observed: &'a ObservedServer, name: &str) -> Option<&'a str> {
    match observed.mssp.get(name)? {
        ObservedValue::Text(text) => Some(text),
        ObservedValue::List(_) => None,
    }
}

/// `PLAYERS` as a count, when it parses as one.
fn players_count(observed: &ObservedServer) -> Option<i64> {
    scalar(observed, "PLAYERS")?
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|count| (0..=MAX_PLAYERS).contains(count))
}

/// Whole days the server has been up, from the `UPTIME` unix start time —
/// computed against `now`, so it is current as long as the server hasn't
/// restarted. Absurd start times (future, or before [`UPTIME_EPOCH_FLOOR`])
/// read as absent.
fn uptime_days(observed: &ObservedServer, now: DateTime<Utc>) -> Option<i64> {
    let start = scalar(observed, "UPTIME")?.trim().parse::<i64>().ok()?;
    if !(UPTIME_EPOCH_FLOOR..=now.timestamp()).contains(&start) {
        return None;
    }
    Some((now.timestamp() - start) / 86_400)
}

/// The advertised TLS port, when one actually was: `SSL` and `TLS` are
/// aliases, and `"0"`/`"1"`/`"-1"` are support flags, not ports (Mudlet's
/// filter, kept exactly).
fn advertised_tls_port(observed: &ObservedServer) -> Option<u16> {
    ["SSL", "TLS"].iter().find_map(|name| {
        let port = scalar(observed, name)?.trim().parse::<u32>().ok()?;
        if port <= 1 || port > 65_535 {
            return None;
        }
        u16::try_from(port).ok()
    })
}

/// Whether TLS was advertised at all: a usable port, or the bare `"1"`
/// support flag — the badge condition, deliberately weaker than an
/// offerable port. `"0"`/`"-1"` say "unsupported" and earn nothing.
fn tls_advertised(observed: &ObservedServer) -> bool {
    advertised_tls_port(observed).is_some()
        || ["SSL", "TLS"]
            .iter()
            .any(|name| scalar(observed, name).is_some_and(|value| value.trim() == "1"))
}

/// The game's own `NAME` when it says something the user's server name
/// doesn't — the muted subtitle. Same-name (case-insensitive) is noise.
fn game_display_name(observed: &ObservedServer, server_name: &str) -> Option<String> {
    let name = scalar(observed, "NAME")?.trim();
    if name.is_empty() || name.eq_ignore_ascii_case(server_name) {
        return None;
    }
    Some(safe_prose_display(name, 60))
}

/// `CONTACT` as an email address, when it is one: a single `@` with a dotted
/// domain and no whitespace or URL scheme. Becomes a `mailto:` action; every
/// other non-URL contact stays inert text.
fn email_contact(observed: &ObservedServer) -> Option<&str> {
    let contact = scalar(observed, "CONTACT")?.trim();
    let (local, domain) = contact.split_once('@')?;
    if local.is_empty()
        || local.contains(':') // scheme-shaped ("mailto:user") — not a bare address
        || domain.is_empty()
        || !domain.contains('.')
        || domain.contains('@')
        || contact.chars().any(char::is_whitespace)
        || link_url_host(contact).is_some()
    {
        return None;
    }
    Some(contact)
}

/// `STATUS` worth a badge: anything but the default "Live".
fn status_text(observed: &ObservedServer) -> Option<String> {
    let status = scalar(observed, "STATUS")?.trim();
    if status.is_empty() || status.eq_ignore_ascii_case("live") {
        return None;
    }
    Some(safe_prose_display(status, 24))
}

/// Compact relative age ("3d ago") for point-in-time observed values.
fn relative_ago(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - then).num_seconds().max(0);
    if seconds < 60 {
        t!("ago-just-now")
    } else if seconds < 3_600 {
        t!("ago-minutes", "minutes" => seconds / 60)
    } else if seconds < 86_400 {
        t!("ago-hours", "hours" => seconds / 3_600)
    } else {
        t!("ago-days", "days" => seconds / 86_400)
    }
}

/// Safe display copy of a server-supplied URL for the confirm dialog:
/// middle-elided, deceptive invisible characters escaped, never
/// server-relabelable.
pub(super) fn safe_url_display(url: &str) -> String {
    let elided = crate::session_store::elide_middle(url, LINK_CONFIRM_DISPLAY_CHARS);
    escape_invisible_text(&elided, InvisiblePolicy::ActionTarget).into_owned()
}

/// Safe display copy of server-supplied prose (names, statuses, contacts).
fn safe_prose_display(text: &str, max: usize) -> String {
    let elided = crate::session_store::elide_middle(text, max);
    escape_invisible_text(&elided, InvisiblePolicy::Prose).into_owned()
}

// --- Views ---

/// The game's own name as a muted subtitle under the server title, when the
/// sidecar has one that differs from the user's name for the server.
pub(super) fn game_name_subtitle<'a>(
    state: &'a State,
    server_name: &str,
) -> Option<Element<'a, Message>> {
    let observed = state.observed.get(server_name)?;
    let name = game_display_name(observed, server_name)?;
    Some(text(name).size(13).style(builtins::text::muted).into())
}

/// The metadata band under the address line: observed facts, then a chip row
/// of badges and link actions. `None` when there is nothing to show — the pane
/// then renders exactly as it always has.
pub(super) fn metadata_band<'a>(
    state: &'a State,
    server: &'a Server,
) -> Option<Element<'a, Message>> {
    let observed = state.observed.get(&server.name)?;
    let now = Utc::now();

    let mut lines: Vec<Element<'a, Message>> = Vec::new();
    if let (Some(players), Some(_)) = (players_count(observed), observed.mssp_received_at) {
        lines.push(muted_line(t!("observed-players", "players" => players)));
    }
    if let Some(last_connected_at) = observed.last_connected_at {
        lines.push(muted_line(t!(
            "observed-last-connected",
            "ago" => relative_ago(last_connected_at, now)
        )));
    }
    if let Some(days) = uptime_days(observed, now) {
        lines.push(muted_line(t!("observed-uptime", "days" => days)));
    }
    // A CONTACT that is neither a URL nor an email is still contact info
    // worth showing — as inert text, never an action. Emails join the chip
    // row below as `mailto:` actions instead.
    if let Some(contact) = scalar(observed, "CONTACT")
        .map(str::trim)
        .filter(|contact| !contact.is_empty() && link_url_host(contact).is_none())
        && email_contact(observed).is_none()
    {
        lines.push(muted_line(
            t!("observed-contact", "contact" => safe_prose_display(contact, 80)),
        ));
    }

    let mut chips = Row::new().spacing(8).align_y(Alignment::Center);
    let mut has_chips = false;
    // A passive nudge toward the encrypted port: shown only while the server
    // config isn't on TLS yet. Advisory only — clicking nothing, changing
    // nothing.
    if !server.config.tls && tls_advertised(observed) {
        chips = chips.push(badge(t!("observed-tls-available")));
        has_chips = true;
    }
    if let Some(status) = status_text(observed) {
        chips = chips.push(badge(status));
        has_chips = true;
    }
    // Server-supplied URLs are exactly OSC 8 links: quiet inline actions
    // behind the same per-server trust gate (`Message::OpenObservedLink`).
    for (variable, label) in [
        ("DISCORD", t!("observed-link-discord")),
        ("WEBSITE", t!("observed-link-website")),
        ("CONTACT", t!("observed-link-contact")),
    ] {
        if let Some(url) = scalar(observed, variable).map(str::trim)
            && link_url_host(url).is_some()
        {
            chips = chips.push(
                button(text(label).size(12))
                    .style(builtins::button::link)
                    .padding([2, 8])
                    .on_press(Message::OpenObservedLink(server.clone(), url.to_string())),
            );
            has_chips = true;
        }
    }
    // An email CONTACT is an action too: a `mailto:` through the same trust
    // gate (no host to grant, so it confirms unless the server has the
    // blanket grant). Labeled with the address itself — clearer than a
    // generic "Contact" for where the click lands.
    if let Some(email) = email_contact(observed) {
        chips = chips.push(
            button(text(safe_prose_display(email, 40)).size(12))
                .style(builtins::button::link)
                .padding([2, 8])
                .on_press(Message::OpenObservedLink(
                    server.clone(),
                    format!("mailto:{email}"),
                )),
        );
        has_chips = true;
    }

    if lines.is_empty() && !has_chips {
        return None;
    }
    let mut band = Column::with_children(lines).spacing(4);
    if has_chips {
        band = band.push(chips);
    }
    // The frame marks the hand-off: everything inside is what the game said
    // (plus the connect stamp), set off from the user-authored config around
    // it — same hairline-and-wash vocabulary as the badges.
    Some(
        container(band)
            .padding(10)
            .width(Length::Fill)
            .style(|theme: &crate::Theme| container::Style {
                background: Some(theme.styles.text.normal.scale_alpha(0.03).into()),
                border: Border {
                    color: theme.styles.general.border,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..Default::default()
            })
            .into(),
    )
}

fn muted_line<'a>(line: String) -> Element<'a, Message> {
    text(line).size(13).style(builtins::text::muted).into()
}

/// A small outlined pill badge (TLS-available, non-Live status).
fn badge<'a>(label: String) -> Element<'a, Message> {
    container(text(label).size(11).style(builtins::text::muted))
        .padding(Padding {
            top: 2.0,
            bottom: 2.0,
            left: 8.0,
            right: 8.0,
        })
        .style(|theme: &crate::Theme| container::Style {
            background: Some(theme.styles.text.normal.scale_alpha(0.04).into()),
            border: Border {
                color: theme.styles.general.border,
                width: 1.0,
                radius: 5.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// The trust-gate dialog for a metadata link — the session view's link
/// dialog with this modal's messages: safely escaped destination, a
/// perform/cancel pair, and the two opt-in grants shared with OSC 8 links.
/// Clicking the dimmed backdrop cancels.
pub(super) fn link_confirm_dialog(pending: &ObservedLinkConfirm) -> Element<'static, Message> {
    let mut body = iced::widget::column![
        text(t!("link-confirm-open-title")).size(15),
        container(text(pending.display.clone()).size(13))
            .padding(8)
            .width(Length::Fill)
            .style(|_: &crate::Theme| container::Style {
                background: Some(Color::from_rgba8(0, 0, 0, 0.35).into()),
                border: Border {
                    radius: 4.0.into(),
                    ..Border::default()
                },
                ..Default::default()
            }),
    ]
    .spacing(12);

    if let Some(host) = &pending.host {
        body = body.push(
            checkbox(pending.grant_host)
                .label(t!("link-confirm-allow-host", "host" => host))
                .on_toggle(Message::ObservedLinkGrantHost)
                .size(15)
                .text_size(13),
        );
    }
    body = body.push(
        checkbox(pending.grant_server)
            .label(t!("link-confirm-trust-server"))
            .on_toggle(Message::ObservedLinkGrantServer)
            .size(15)
            .text_size(13),
    );
    body = body.push(
        row![
            space::horizontal(),
            button(text(t!("action-cancel")).size(13))
                .style(builtins::button::subtle)
                .padding([4, 14])
                .on_press(Message::ObservedLinkCancel),
            button(text(t!("action-open")).size(13))
                .padding([4, 14])
                .on_press(Message::ObservedLinkProceed),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    );

    let card = container(body)
        .padding(16)
        .max_width(560)
        .style(|_: &crate::Theme| container::Style {
            background: Some(Color::from_rgba8(32, 32, 38, 1.0).into()),
            border: Border {
                radius: 8.0.into(),
                color: Color::from_rgba8(255, 255, 255, 0.12),
                width: 1.0,
            },
            ..Default::default()
        });

    opaque(
        mouse_area(
            center(opaque(card)).style(|_: &crate::Theme| container::Style {
                background: Some(Color::from_rgba8(0, 0, 0, 0.55).into()),
                ..Default::default()
            }),
        )
        .on_press(Message::ObservedLinkCancel),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn observed(pairs: &[(&str, &str)]) -> ObservedServer {
        ObservedServer {
            mssp: pairs
                .iter()
                .map(|(name, value)| {
                    (
                        (*name).to_string(),
                        ObservedValue::Text((*value).to_string()),
                    )
                })
                .collect::<BTreeMap<_, _>>(),
            ..ObservedServer::default()
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_760_000_000, 0).expect("valid timestamp")
    }

    #[test]
    fn players_parse_at_the_display_edge_only() {
        assert_eq!(players_count(&observed(&[("PLAYERS", "52")])), Some(52));
        assert_eq!(players_count(&observed(&[("PLAYERS", " 7 ")])), Some(7));
        // Unparsable, negative, and absurd counts render as absent — never
        // as an error.
        assert_eq!(players_count(&observed(&[("PLAYERS", "many")])), None);
        assert_eq!(players_count(&observed(&[("PLAYERS", "-3")])), None);
        assert_eq!(players_count(&observed(&[("PLAYERS", "99999999")])), None);
        assert_eq!(players_count(&observed(&[])), None);
    }

    #[test]
    fn uptime_is_a_start_time_and_absurd_values_render_absent() {
        let start = now().timestamp() - 3 * 86_400 - 60;
        assert_eq!(
            uptime_days(&observed(&[("UPTIME", &start.to_string())]), now()),
            Some(3)
        );
        // The future and the pre-1990 past are lies.
        let future = now().timestamp() + 60;
        assert_eq!(
            uptime_days(&observed(&[("UPTIME", &future.to_string())]), now()),
            None
        );
        assert_eq!(uptime_days(&observed(&[("UPTIME", "100")]), now()), None);
        assert_eq!(uptime_days(&observed(&[("UPTIME", "soon")]), now()), None);
    }

    #[test]
    fn tls_flags_are_not_ports() {
        assert_eq!(
            advertised_tls_port(&observed(&[("SSL", "6667")])),
            Some(6667)
        );
        // "TLS" is an alias for "SSL".
        assert_eq!(
            advertised_tls_port(&observed(&[("TLS", "6667")])),
            Some(6667)
        );
        // Support flags and out-of-range values are not ports.
        assert_eq!(advertised_tls_port(&observed(&[("SSL", "1")])), None);
        assert_eq!(advertised_tls_port(&observed(&[("SSL", "-1")])), None);
        assert_eq!(advertised_tls_port(&observed(&[("SSL", "0")])), None);
        assert_eq!(advertised_tls_port(&observed(&[("SSL", "99999")])), None);
        // A flag under one alias doesn't hide a real port under the other.
        assert_eq!(
            advertised_tls_port(&observed(&[("SSL", "1"), ("TLS", "6667")])),
            Some(6667)
        );
    }

    #[test]
    fn the_bare_support_flag_still_advertises_tls() {
        // "SSL 1" means "TLS supported" without naming a port: badge-worthy,
        // never offer-worthy.
        assert!(tls_advertised(&observed(&[("SSL", "1")])));
        assert!(tls_advertised(&observed(&[("TLS", "6667")])));
        // "0"/"-1" say "unsupported", and garbage says nothing.
        assert!(!tls_advertised(&observed(&[("SSL", "0")])));
        assert!(!tls_advertised(&observed(&[("SSL", "-1")])));
        assert!(!tls_advertised(&observed(&[("SSL", "yes")])));
        assert!(!tls_advertised(&observed(&[])));
    }

    #[test]
    fn the_game_name_subtitle_only_appears_when_it_adds_information() {
        assert_eq!(
            game_display_name(&observed(&[("NAME", "ArcticMUD")]), "Arctic"),
            Some("ArcticMUD".to_string())
        );
        // Matching the user's name (case-insensitively) is noise.
        assert_eq!(
            game_display_name(&observed(&[("NAME", "arctic")]), "Arctic"),
            None
        );
        assert_eq!(
            game_display_name(&observed(&[("NAME", "  ")]), "Arctic"),
            None
        );
        assert_eq!(game_display_name(&observed(&[]), "Arctic"), None);
    }

    #[test]
    fn only_a_non_live_status_earns_a_badge() {
        assert_eq!(
            status_text(&observed(&[("STATUS", "Closed Beta")])),
            Some("Closed Beta".to_string())
        );
        assert_eq!(status_text(&observed(&[("STATUS", "Live")])), None);
        assert_eq!(status_text(&observed(&[("STATUS", "LIVE")])), None);
        assert_eq!(status_text(&observed(&[])), None);
    }

    #[test]
    fn relative_ages_step_through_their_units() {
        let then = |seconds: i64| {
            DateTime::from_timestamp(now().timestamp() - seconds, 0).expect("valid timestamp")
        };
        assert_eq!(relative_ago(then(5), now()), "just now");
        assert_eq!(relative_ago(then(120), now()), "2m ago");
        assert_eq!(relative_ago(then(2 * 3_600), now()), "2h ago");
        assert_eq!(relative_ago(then(3 * 86_400), now()), "3d ago");
        // A future timestamp (clock skew) clamps rather than inventing time.
        assert_eq!(relative_ago(then(-60), now()), "just now");
    }

    #[test]
    fn hostile_text_is_escaped_before_display() {
        assert_eq!(
            game_display_name(&observed(&[("NAME", "Evil\u{202e}MUD")]), "Arctic"),
            Some("Evil\\u{202E}MUD".to_string())
        );
    }

    #[test]
    fn only_a_bare_address_contact_reads_as_email() {
        assert_eq!(
            email_contact(&observed(&[("CONTACT", " support@stickmud.com ")])),
            Some("support@stickmud.com")
        );
        // URLs keep their link chip; scheme-shaped and malformed values
        // never become a `mailto:` target.
        for contact in [
            "https://stickmud.com/contact",
            "mailto:support@stickmud.com",
            "support@stickmud",
            "@stickmud.com",
            "support@",
            "sup port@stickmud.com",
            "a@b@stickmud.com",
            "the wizard on channel gossip",
        ] {
            assert_eq!(
                email_contact(&observed(&[("CONTACT", contact)])),
                None,
                "{contact}"
            );
        }
    }
}
