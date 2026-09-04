//! Bottom-anchored action toasts for the main session window.
//!
//! The window has one visible toast slot; further toasts wait in a FIFO queue behind it.
//! Both payload kinds come from the background package-update checker: the consolidated
//! updates-ready offer ("reload now or just keep playing") and the per-package
//! needs-permissions offer (review, pin, or dismiss). When more than two
//! needs-permissions offers are pending for one server, they collapse into a single
//! "updates need attention" toast whose one action opens the Automations window — the
//! per-package update cards already live there, and a parade of toasts would be noise.
//!
//! The layer is non-modal: no backdrop, no click interception — the window behind it
//! stays fully interactive, and only the pill's own buttons take input.

use std::collections::VecDeque;

use iced::alignment::Vertical;
use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Length, Padding};

use smudgy_core::models::package_updates::PackageVersionRef;
use smudgy_core::models::shared_packages::{LockedPackage, PackagePermissions};

use crate::theme::{self, Element, Theme};

/// One needs-permissions update offer, fully described: everything the toast, the
/// review modal, and an eventual grant need to render and act without another network
/// check. Built by the update checker from a `check-updates` result and the entry's
/// lockfile state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOffer {
    /// The server whose lockfile installs the package.
    pub server_name: String,
    /// The complete lock row from which this offer was evaluated. Granting compares this snapshot
    /// before it changes consent or stages content, so an old toast cannot modify a reinstalled or
    /// concurrently edited package.
    pub expected: LockedPackage,
    /// The lock entry's specifier (`smudgy://owner/name`).
    pub specifier: String,
    /// The package's display name (the specifier's name segment).
    pub name: String,
    /// The currently staged version, when one exists.
    pub current: Option<String>,
    /// The offered (latest live) version.
    pub latest: String,
    /// Permissions the offered closure union asks for beyond the consented grant.
    pub added: PackagePermissions,
    /// The offered version's whole-closure union — what a grant records.
    pub new_union: PackagePermissions,
    /// The offered version's transitive dependency closure at locked versions (root
    /// excluded) — what a grant must resolve and prefetch before staging.
    pub closure: Vec<PackageVersionRef>,
    /// The smudgy version the offered closure requires when it exceeds the running
    /// one. The checker never offers such an update (the version floor takes
    /// precedence and the pane card covers it), so this is a belt-and-braces field:
    /// the review modal withholds Grant and explains whenever it is set.
    pub needs_smudgy: Option<String>,
}

/// One queued toast.
#[derive(Debug, Clone)]
pub enum Toast {
    /// Every pending within-consent update for `server_name` is staged and prefetched:
    /// offer a live reload (near-instant — the reload serves the staged versions from
    /// cache). Ignoring is safe; staging is durable and applies at the next load anyway.
    UpdatesReady { server_name: String, count: usize },
    /// One package's update needs permissions beyond the consented grant.
    NeedsPermissions(Box<UpdateOffer>),
    /// The collapsed form of more than two pending [`Toast::NeedsPermissions`] for one
    /// server; its single action opens the Automations window. Carries the folded
    /// specifiers (count = length) so a later duplicate offer is recognized rather
    /// than inflating the count.
    NeedsAttention {
        server_name: String,
        specifiers: Vec<String>,
    },
    /// A granted update's prefetch or atomic lock commit failed. The lock row stays unchanged, but
    /// the user acted, so the failure must be visible rather than only a log line. Dismiss-only.
    StageFailed { server_name: String, name: String },
}

impl Toast {
    /// The server a needs-permissions (or collapsed) toast concerns; `None` for the
    /// toasts that never participate in collapsing.
    fn needs_permissions_server(&self) -> Option<&str> {
        match self {
            Toast::NeedsPermissions(offer) => Some(&offer.server_name),
            Toast::NeedsAttention { server_name, .. } => Some(server_name),
            Toast::UpdatesReady { .. } | Toast::StageFailed { .. } => None,
        }
    }
}

/// A toast button press. The window maps these into its own message type and answers
/// with events for the daemon; the toast itself only manages its slot + queue.
#[derive(Debug, Clone)]
pub enum Message {
    /// The updates-ready toast's primary action: reload every session on the server so
    /// they pick the staged versions up from cache.
    ReloadScripts { server_name: String },
    /// The updates-ready toast's dismissal. Nothing persists — staging is durable, so
    /// the updates apply at the next session load regardless.
    Ignore,
    /// The needs-permissions toast's primary action: open the review modal.
    Review(Box<UpdateOffer>),
    /// Pin the package at its currently staged version — the terminal "stop asking"
    /// answer that also ends the per-load delta scans.
    PinCurrent {
        server_name: String,
        expected: LockedPackage,
        specifier: String,
        version: String,
    },
    /// Dismiss the offer until a strictly newer version appears (persisted).
    Later {
        server_name: String,
        expected: LockedPackage,
        specifier: String,
        version: String,
    },
    /// The collapsed needs-attention toast's single action.
    OpenAutomations { server_name: String },
}

/// The window's toast state: one visible slot plus the FIFO queue behind it.
#[derive(Debug, Default)]
pub struct Toasts {
    current: Option<Toast>,
    queue: VecDeque<Toast>,
}

/// The pending needs-permissions count (however represented) above which a server's
/// offers collapse into one needs-attention toast.
const COLLAPSE_THRESHOLD: usize = 2;

impl Toasts {
    /// Queue a toast, displaying it immediately when the slot is free. A duplicate of
    /// a toast already queued or showing is absorbed first (see
    /// [`absorb_duplicate`](Self::absorb_duplicate)). Pushing a needs-permissions
    /// offer re-checks the collapse rule for its server: more than
    /// [`COLLAPSE_THRESHOLD`] pending offers fold into one needs-attention toast (an
    /// existing needs-attention toast for the server absorbs later offers directly).
    pub fn push(&mut self, toast: Toast) {
        if self.absorb_duplicate(&toast) {
            return;
        }
        let collapse_server = match &toast {
            Toast::NeedsPermissions(offer) => Some(offer.server_name.clone()),
            Toast::UpdatesReady { .. }
            | Toast::NeedsAttention { .. }
            | Toast::StageFailed { .. } => None,
        };
        if self.current.is_none() {
            self.current = Some(toast);
        } else {
            self.queue.push_back(toast);
        }
        if let Some(server) = collapse_server {
            self.collapse_for(&server);
        }
    }

    /// Drop the visible toast and promote the next queued one.
    pub fn advance(&mut self) {
        self.current = self.queue.pop_front();
    }

    /// Whether `toast` duplicates one already queued or showing — two same-server
    /// sessions opening together feed the checker's toasts twice, and a parade of
    /// identical pills (or an inflated collapse count) would misreport how many
    /// updates exist. A duplicate REPLACES the standing toast in place (the newer
    /// payload is at least as fresh: an updates-ready count, a regenerated offer) —
    /// or is dropped outright when a collapsed needs-attention toast already folds
    /// the same offer.
    fn absorb_duplicate(&mut self, toast: &Toast) -> bool {
        match toast {
            Toast::NeedsPermissions(offer) => {
                for slot in self.slots_mut() {
                    match slot {
                        Toast::NeedsPermissions(existing)
                            if existing.server_name == offer.server_name
                                && existing.specifier == offer.specifier =>
                        {
                            *slot = toast.clone();
                            return true;
                        }
                        Toast::NeedsAttention {
                            server_name,
                            specifiers,
                        } if *server_name == offer.server_name
                            && specifiers.contains(&offer.specifier) =>
                        {
                            return true;
                        }
                        _ => {}
                    }
                }
                false
            }
            Toast::UpdatesReady { server_name, .. } => {
                let server = server_name.clone();
                self.replace_first(toast, |slot| {
                    matches!(slot, Toast::UpdatesReady { server_name, .. } if *server_name == server)
                })
            }
            Toast::StageFailed { server_name, name } => {
                let (server, name) = (server_name.clone(), name.clone());
                self.replace_first(toast, |slot| {
                    matches!(slot, Toast::StageFailed { server_name, name: n }
                        if *server_name == server && *n == name)
                })
            }
            // Built only by the collapse below, never pushed from outside.
            Toast::NeedsAttention { .. } => false,
        }
    }

    /// Replace the first queued-or-showing toast matching `matches` with `toast`.
    fn replace_first(&mut self, toast: &Toast, matches: impl Fn(&Toast) -> bool) -> bool {
        for slot in self.slots_mut() {
            if matches(slot) {
                *slot = toast.clone();
                return true;
            }
        }
        false
    }

    fn slots_mut(&mut self) -> impl Iterator<Item = &mut Toast> {
        self.current.iter_mut().chain(self.queue.iter_mut())
    }

    /// Fold `server`'s pending needs-permissions offers into one needs-attention toast
    /// once they number more than [`COLLAPSE_THRESHOLD`]. The collapsed toast takes the
    /// earliest folded position (the visible slot, if an offer was showing), keeping
    /// arrival order for everything else.
    fn collapse_for(&mut self, server: &str) {
        let mut pending: Vec<String> = Vec::new();
        for toast in self.current.iter().chain(self.queue.iter()) {
            match toast {
                Toast::NeedsPermissions(offer) if offer.server_name == server => {
                    if !pending.contains(&offer.specifier) {
                        pending.push(offer.specifier.clone());
                    }
                }
                Toast::NeedsAttention {
                    server_name,
                    specifiers,
                } if server_name == server => {
                    for specifier in specifiers {
                        if !pending.contains(specifier) {
                            pending.push(specifier.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        if pending.len() <= COLLAPSE_THRESHOLD {
            return;
        }
        let collapsed = Toast::NeedsAttention {
            server_name: server.to_string(),
            specifiers: pending,
        };
        // Rebuild slot + queue in order, replacing the first folded toast with the
        // collapsed one and dropping the rest.
        let ordered: Vec<Toast> = self
            .current
            .take()
            .into_iter()
            .chain(self.queue.drain(..))
            .collect();
        let mut replaced = false;
        for toast in ordered {
            if toast.needs_permissions_server() == Some(server) {
                if !replaced {
                    replaced = true;
                    self.enqueue_in_order(collapsed.clone());
                }
                continue;
            }
            self.enqueue_in_order(toast);
        }
    }

    fn enqueue_in_order(&mut self, toast: Toast) {
        if self.current.is_none() && self.queue.is_empty() {
            self.current = Some(toast);
        } else {
            self.queue.push_back(toast);
        }
    }

    /// The toast layer for the window `stack`: a full-bleed, non-capturing container
    /// with the pill bottom-centered. `None` while no toast is showing, so the window
    /// skips the layer entirely.
    pub fn view(&self) -> Option<Element<'_, Message>> {
        let toast = self.current.as_ref()?;
        let content: Element<'_, Message> = match toast {
            Toast::UpdatesReady { server_name, count } => row![
                text(crate::i18n::t!(
                    "toast-package-updates-ready",
                    "count" => count_arg(*count)
                ))
                .size(13.0),
                action_button(
                    crate::i18n::t!("toast-package-reload-scripts"),
                    theme::builtins::button::primary,
                    Message::ReloadScripts {
                        server_name: server_name.clone(),
                    },
                ),
                action_button(
                    crate::i18n::t!("toast-package-ignore"),
                    theme::builtins::button::link,
                    Message::Ignore,
                ),
            ]
            .spacing(10.0)
            .align_y(Vertical::Center)
            .into(),
            Toast::NeedsPermissions(offer) => {
                let mut actions = row![
                    text(crate::i18n::t!(
                        "toast-package-needs-permissions",
                        "name" => offer.name.as_str()
                    ))
                    .size(13.0),
                    action_button(
                        crate::i18n::t!("toast-package-review"),
                        theme::builtins::button::primary,
                        Message::Review(offer.clone()),
                    ),
                ]
                .spacing(10.0)
                .align_y(Vertical::Center);
                // Pinning needs a concrete version to pin; a never-resolved install
                // has none, so the offer simply doesn't carry the button.
                if let Some(current) = &offer.current {
                    actions = actions.push(action_button(
                        crate::i18n::t!("toast-package-pin-current"),
                        theme::builtins::button::secondary,
                        Message::PinCurrent {
                            server_name: offer.server_name.clone(),
                            expected: offer.expected.clone(),
                            specifier: offer.specifier.clone(),
                            version: current.clone(),
                        },
                    ));
                }
                actions
                    .push(action_button(
                        crate::i18n::t!("toast-package-later"),
                        theme::builtins::button::link,
                        Message::Later {
                            server_name: offer.server_name.clone(),
                            expected: offer.expected.clone(),
                            specifier: offer.specifier.clone(),
                            version: offer.latest.clone(),
                        },
                    ))
                    .into()
            }
            Toast::NeedsAttention {
                server_name,
                specifiers,
            } => row![
                text(crate::i18n::t!(
                    "toast-package-attention",
                    "count" => count_arg(specifiers.len())
                ))
                .size(13.0),
                action_button(
                    crate::i18n::t!("toast-package-open-automations"),
                    theme::builtins::button::primary,
                    Message::OpenAutomations {
                        server_name: server_name.clone(),
                    },
                ),
            ]
            .spacing(10.0)
            .align_y(Vertical::Center)
            .into(),
            Toast::StageFailed { name, .. } => row![
                text(crate::i18n::t!(
                    "toast-package-update-failed",
                    "name" => name.as_str()
                ))
                .size(13.0),
                action_button(
                    crate::i18n::t!("action-dismiss"),
                    theme::builtins::button::link,
                    Message::Ignore,
                ),
            ]
            .spacing(10.0)
            .align_y(Vertical::Center)
            .into(),
        };

        let pill = container(content)
            .padding(Padding {
                top: 8.0,
                bottom: 8.0,
                left: 16.0,
                right: 16.0,
            })
            .style(pill_style);
        Some(
            container(column![iced::widget::space::vertical(), pill].align_x(Alignment::Center))
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(20)
                .into(),
        )
    }
}

/// A toast action button: small label, tight padding, the given built-in style.
fn action_button<'a>(
    label: String,
    style: fn(&Theme, iced::widget::button::Status) -> iced::widget::button::Style,
    message: Message,
) -> Element<'a, Message> {
    button(text(label).size(12.0))
        .style(style)
        .padding([4, 10])
        .on_press(message)
        .into()
}

/// The pill surface: the same raised-modal-body recipe as the Automations window's
/// status pill, so toasts read as one family across windows.
fn pill_style(theme: &Theme) -> iced::widget::container::Style {
    iced::widget::container::Style {
        background: Some(theme.styles.modal.body_background),
        border: theme.styles.modal.body_border,
        shadow: theme.styles.modal.shadow,
        ..Default::default()
    }
}

/// A count as a Fluent numeric argument (saturating on the absurd overflow).
fn count_arg(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use smudgy_core::models::shared_packages::UpdateMode;

    use super::*;

    fn offer(server: &str, name: &str) -> Toast {
        Toast::NeedsPermissions(Box::new(UpdateOffer {
            server_name: server.to_string(),
            expected: LockedPackage::new(format!("smudgy://wbk/{name}"), UpdateMode::Auto),
            specifier: format!("smudgy://wbk/{name}"),
            name: name.to_string(),
            current: Some("1.0.0".into()),
            latest: "2.0.0".into(),
            added: PackagePermissions::default(),
            new_union: PackagePermissions::default(),
            closure: Vec::new(),
            needs_smudgy: None,
        }))
    }

    fn ready(server: &str, count: usize) -> Toast {
        Toast::UpdatesReady {
            server_name: server.to_string(),
            count,
        }
    }

    #[test]
    fn single_slot_with_fifo_queue() {
        let mut toasts = Toasts::default();
        toasts.push(ready("arctic", 2));
        toasts.push(offer("arctic", "mapper"));
        assert!(matches!(
            toasts.current.as_ref(),
            Some(Toast::UpdatesReady { count: 2, .. })
        ));
        toasts.advance();
        assert!(matches!(
            toasts.current.as_ref(),
            Some(Toast::NeedsPermissions(_))
        ));
        toasts.advance();
        assert!(toasts.current.as_ref().is_none());
    }

    #[test]
    fn two_offers_stay_individual() {
        let mut toasts = Toasts::default();
        toasts.push(offer("arctic", "mapper"));
        toasts.push(offer("arctic", "duo"));
        assert!(matches!(
            toasts.current.as_ref(),
            Some(Toast::NeedsPermissions(_))
        ));
        toasts.advance();
        assert!(matches!(
            toasts.current.as_ref(),
            Some(Toast::NeedsPermissions(_))
        ));
    }

    #[test]
    fn more_than_two_offers_collapse_into_needs_attention() {
        let mut toasts = Toasts::default();
        toasts.push(offer("arctic", "mapper"));
        toasts.push(offer("arctic", "duo"));
        toasts.push(offer("arctic", "chat"));
        // All three fold into one toast at the earliest (visible) position.
        match toasts.current.as_ref() {
            Some(Toast::NeedsAttention {
                server_name,
                specifiers,
            }) => {
                assert_eq!(server_name, "arctic");
                assert_eq!(specifiers.len(), 3);
            }
            other => panic!("expected the collapsed toast, got {other:?}"),
        }
        toasts.advance();
        assert!(toasts.current.as_ref().is_none());
    }

    #[test]
    fn collapsed_toast_absorbs_later_offers() {
        let mut toasts = Toasts::default();
        for name in ["a", "b", "c", "d"] {
            toasts.push(offer("arctic", name));
        }
        match toasts.current.as_ref() {
            Some(Toast::NeedsAttention { specifiers, .. }) => assert_eq!(specifiers.len(), 4),
            other => panic!("expected the collapsed toast, got {other:?}"),
        }
    }

    #[test]
    fn collapse_is_per_server_and_leaves_other_toasts_in_order() {
        let mut toasts = Toasts::default();
        toasts.push(ready("frostfell", 1));
        toasts.push(offer("arctic", "a"));
        toasts.push(offer("frostfell", "x"));
        toasts.push(offer("arctic", "b"));
        toasts.push(offer("arctic", "c"));
        // arctic collapses; frostfell's single offer and the ready toast survive.
        assert!(matches!(
            toasts.current.as_ref(),
            Some(Toast::UpdatesReady { .. })
        ));
        toasts.advance();
        match toasts.current.as_ref() {
            Some(Toast::NeedsAttention {
                server_name,
                specifiers,
            }) => {
                assert_eq!(server_name, "arctic");
                assert_eq!(specifiers.len(), 3);
            }
            other => panic!("expected arctic's collapsed toast, got {other:?}"),
        }
        toasts.advance();
        match toasts.current.as_ref() {
            Some(Toast::NeedsPermissions(offer)) => assert_eq!(offer.server_name, "frostfell"),
            other => panic!("expected frostfell's offer, got {other:?}"),
        }
    }

    #[test]
    fn a_duplicate_offer_replaces_the_standing_one() {
        // Two same-server sessions opening together feed identical offers; the
        // second replaces the first in place — one pill, and no phantom third
        // offer to trip the collapse threshold.
        let mut toasts = Toasts::default();
        toasts.push(offer("arctic", "mapper"));
        toasts.push(offer("arctic", "duo"));
        toasts.push(offer("arctic", "mapper"));
        assert!(matches!(
            toasts.current.as_ref(),
            Some(Toast::NeedsPermissions(o)) if o.name == "mapper"
        ));
        assert_eq!(toasts.queue.len(), 1, "the duplicate replaced, not queued");
        // A replacement carries the fresher payload (a regenerated offer).
        let mut fresher = offer("arctic", "duo");
        if let Toast::NeedsPermissions(o) = &mut fresher {
            o.latest = "3.0.0".into();
        }
        toasts.push(fresher);
        match toasts.queue.front() {
            Some(Toast::NeedsPermissions(o)) => assert_eq!(o.latest, "3.0.0"),
            other => panic!("expected the replaced offer, got {other:?}"),
        }
    }

    #[test]
    fn an_offer_already_folded_into_attention_is_dropped() {
        let mut toasts = Toasts::default();
        for name in ["a", "b", "c"] {
            toasts.push(offer("arctic", name));
        }
        toasts.push(offer("arctic", "b"));
        match toasts.current.as_ref() {
            Some(Toast::NeedsAttention { specifiers, .. }) => {
                assert_eq!(
                    specifiers.len(),
                    3,
                    "the folded duplicate does not inflate the count"
                );
            }
            other => panic!("expected the collapsed toast, got {other:?}"),
        }
        assert!(toasts.queue.is_empty());
    }

    #[test]
    fn a_duplicate_updates_ready_replaces_per_server() {
        let mut toasts = Toasts::default();
        toasts.push(ready("arctic", 2));
        toasts.push(offer("arctic", "mapper"));
        // The same server's re-check replaces the standing ready toast (with its
        // fresher count) instead of queuing a second one...
        toasts.push(ready("arctic", 3));
        match toasts.current.as_ref() {
            Some(Toast::UpdatesReady { count, .. }) => assert_eq!(*count, 3),
            other => panic!("expected the replaced ready toast, got {other:?}"),
        }
        assert_eq!(toasts.queue.len(), 1);
        // ...while another server's ready toast queues normally.
        toasts.push(ready("frostfell", 1));
        assert_eq!(toasts.queue.len(), 2);
    }

    #[test]
    fn a_duplicate_stage_failure_replaces() {
        let mut toasts = Toasts::default();
        let failed = || Toast::StageFailed {
            server_name: "arctic".into(),
            name: "mapper".into(),
        };
        toasts.push(failed());
        toasts.push(failed());
        assert!(matches!(
            toasts.current.as_ref(),
            Some(Toast::StageFailed { .. })
        ));
        assert!(toasts.queue.is_empty(), "one failure, one pill");
    }
}
