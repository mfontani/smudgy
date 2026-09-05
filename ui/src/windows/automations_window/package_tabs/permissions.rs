//! Permissions-tab sandbox and grant renderers.

use super::super::packages::sandbox_summary;
use super::*;
use crate::components::permissions::{
    PermissionRisk, consent_can_row, full_access_banner, permission_can_lines, union_risk,
};
use iced::widget::column;
use smudgy_core::models::local_packages::LocalPackage;

impl AutomationsWindow {
    pub(super) fn view_owned_sandbox_section(&self, package: &LocalPackage) -> Elem<'_> {
        let own_spec = self.local_own_spec(&package.name);
        let unsandboxed = self
            .installed_packages
            .iter()
            .find(|p| p.specifier == own_spec)
            .is_some_and(|p| p.trusted);

        let mut col = Column::new()
            .spacing(8.0)
            .push(common::section_label(crate::i18n::ts!("package-sandbox")));

        if unsandboxed {
            col = col.push(
                container(
                    column![
                        row![
                            text("\u{26A0}").size(14.0).style(common::danger),
                            text(crate::i18n::t!("package-developing-unsandboxed")).size(14.0),
                        ]
                        .spacing(8.0)
                        .align_y(Vertical::Center),
                        text(crate::i18n::t!("package-unsandboxed-owned-help"))
                            .size(12.0)
                            .style(common::muted),
                    ]
                    .spacing(6.0),
                )
                .padding(12.0)
                .width(Length::Fill)
                .style(common::banner_style),
            );
            // Re-sandboxing is the safe direction — always offered, even with advanced features off.
            col = col.push(
                row![
                    iced::widget::space::horizontal(),
                    button(text(crate::i18n::t!("package-use-manifest-sandbox")).size(12.0))
                        .style(button_style::secondary)
                        .on_press(Message::SetLocalUnsandboxed(false)),
                ]
                .align_y(Vertical::Center),
            );
            return col.into();
        }

        // Sandboxed against the live manifest: show what it currently grants (reusing the consent
        // can-lines), and point at the manifest editor as the grant mechanism. The full-access
        // banner shows here too — the author sees exactly the framing installers will get.
        let can = permission_can_lines(&package.manifest.permissions);
        let mut card =
            column![text(crate::i18n::t!("package-runs-manifest-sandbox")).size(14.0)].spacing(6.0);
        if can.is_empty() {
            card = card.push(text(sandbox_summary()).size(12.0).style(common::muted));
        } else {
            if let Some(banner) = full_access_banner(&package.manifest.permissions) {
                card = card.push(banner);
            }
            card = card.push(
                text(crate::i18n::t!("package-it-can"))
                    .size(12.0)
                    .style(common::muted),
            );
            let mut lines = Column::new().spacing(4.0);
            for line in &can {
                lines = lines.push(consent_can_row(line));
            }
            card = card.push(lines);
        }
        card = card.push(
            row![
                button(text(crate::i18n::t!("package-edit-capabilities")).size(12.0))
                    .style(button_style::secondary)
                    .on_press(Message::EditOwnedCapabilities),
            ]
            .align_y(Vertical::Center),
        );
        col = col.push(
            container(card)
                .padding(12.0)
                .width(Length::Fill)
                .style(common::banner_style),
        );

        // Advanced escape hatch: develop with full access (trust), for ffi/run etc. a sandbox can't
        // grant. Gated on advanced features + a heavy two-step confirm (reusing the trust confirm
        // state; only one package pane shows at a time).
        if self.advanced_features {
            if self.confirm_trust {
                col = col.push(
                    container(
                        column![
                            row![
                                text("\u{26A0}").size(14.0).style(common::danger),
                                text(crate::i18n::t!("package-develop-unsandboxed-question"))
                                    .size(14.0),
                            ]
                            .spacing(8.0)
                            .align_y(Vertical::Center),
                            text(crate::i18n::t!("package-develop-unsandboxed-warning")).size(12.0),
                            row![
                                iced::widget::space::horizontal(),
                                button(text(crate::i18n::t!("action-cancel")).size(12.0))
                                    .style(button_style::secondary)
                                    .on_press(Message::CancelTrust),
                                button(
                                    text(crate::i18n::t!("package-develop-unsandboxed")).size(12.0)
                                )
                                .style(button_style::primary)
                                .on_press(Message::SetLocalUnsandboxed(true)),
                            ]
                            .spacing(8.0)
                            .align_y(Vertical::Center),
                        ]
                        .spacing(10.0),
                    )
                    .padding(12.0)
                    .width(Length::Fill)
                    .style(common::banner_style),
                );
            } else {
                col = col.push(
                    row![
                        column![
                            text(crate::i18n::t!("package-develop-unsandboxed-advanced"))
                                .size(13.0),
                            text(crate::i18n::t!("package-develop-unsandboxed-help"))
                                .size(11.0)
                                .style(common::muted),
                        ]
                        .spacing(2.0),
                        iced::widget::space::horizontal(),
                        button(
                            text(crate::i18n::t!("package-develop-unsandboxed-ellipsis"))
                                .size(12.0)
                        )
                        .style(button_style::secondary)
                        .on_press(Message::RequestTrust),
                    ]
                    .align_y(Vertical::Center),
                );
            }
        }
        col.into()
    }

    /// The manage-pane permission view: the consented closure union read-only (all-or-nothing,
    /// so no per-permission revoke), or "full access (trusted)" — plus the trust toggle.
    /// The "Permissions" card for a dependency-reference view. A dependency isn't its own
    /// sandboxed package: it loads into its parent's isolate and runs with the parent's grants, so
    /// it has no separate consent of its own. Describing its manifest permissions here (as the
    /// installed pane does) would imply a sandbox and a grant/keep choice that don't exist in this
    /// context — so explain the parent relationship in plain terms and send the user there instead.
    pub(super) fn view_dependency_permissions_section(&self, parent: &str) -> Elem<'_> {
        let parent_name = package_display_name(parent).to_string();
        let card = column![
            text(crate::i18n::t!("package-runs-inside", "parent" => &parent_name)).size(14.0),
            text(crate::i18n::t!(
                "package-dependency-permissions-help",
                "parent" => &parent_name
            ))
            .size(12.0)
            .style(common::muted),
        ]
        .spacing(6.0);
        column![
            common::section_label(crate::i18n::ts!("manifest-permissions")),
            container(card)
                .padding(12.0)
                .width(Length::Fill)
                .style(common::banner_style),
        ]
        .spacing(8.0)
        .into()
    }

    pub(super) fn view_permissions_section(&self, locked: &LockedPackage) -> Elem<'_> {
        let mut col = Column::new()
            .spacing(8.0)
            .push(common::section_label(crate::i18n::ts!(
                "manifest-permissions"
            )));

        if locked.trusted {
            col = col.push(
                container(
                    column![
                        row![
                            text("\u{26A0}").size(14.0).style(common::danger),
                            text(crate::i18n::t!("package-full-access")).size(14.0),
                        ]
                        .spacing(8.0)
                        .align_y(Vertical::Center),
                        text(crate::i18n::t!("package-full-access-help"))
                            .size(12.0)
                            .style(common::muted),
                    ]
                    .spacing(6.0),
                )
                .padding(12.0)
                .width(Length::Fill)
                .style(common::banner_style),
            );
            // Restoring the sandbox is the safe direction — always offered, even with advanced
            // features off (so a package can't get stuck unsandboxed if the gate is later disabled).
            col = col.push(
                row![
                    iced::widget::space::horizontal(),
                    button(text(crate::i18n::t!("package-restore-sandbox")).size(12.0))
                        .style(button_style::secondary)
                        .on_press(Message::SetTrusted(false)),
                ]
                .align_y(Vertical::Center),
            );
            return col.into();
        }

        // Sandboxed: mirror the trusted card — a heading plus a breakdown of the consented access
        // (read-only; the union is whatever was granted at install). A consented sandbox-escape
        // grant keeps its banner here too: "Runs in sandbox" must not read as containment the
        // grant no longer provides.
        let consented = locked.consented_permissions.clone().unwrap_or_default();
        let can = permission_can_lines(&consented);
        let heading = if union_risk(&consented) == PermissionRisk::Critical {
            crate::i18n::t!("package-runs-sandbox-with-escape-grants")
        } else {
            crate::i18n::t!("package-runs-sandbox")
        };
        let mut card = column![text(heading).size(14.0)].spacing(6.0);
        if can.is_empty() {
            card = card.push(text(sandbox_summary()).size(12.0).style(common::muted));
        } else {
            if let Some(banner) = full_access_banner(&consented) {
                card = card.push(banner);
            }
            card = card.push(
                text(crate::i18n::t!("package-it-can-only"))
                    .size(12.0)
                    .style(common::muted),
            );
            let mut lines = Column::new().spacing(4.0);
            for line in &can {
                lines = lines.push(consent_can_row(line));
            }
            card = card.push(lines);
        }
        if locked.consented_permissions.is_none() {
            card = card.push(
                text(crate::i18n::t!("package-not-consented"))
                    .size(11.0)
                    .style(common::faint),
            );
        }
        col = col.push(
            container(card)
                .padding(12.0)
                .width(Length::Fill)
                .style(common::banner_style),
        );

        // "Remove sandbox" is an advanced, footgun-prone action (run the package with full
        // authority), so the affordance only appears when advanced scripting features are unlocked
        // in Settings. The heavy two-step confirm applies.
        if self.advanced_features {
            if self.confirm_trust {
                col = col.push(
                    container(
                        column![
                            row![
                                text("\u{26A0}").size(14.0).style(common::danger),
                                text(crate::i18n::t!("package-remove-sandbox-question")).size(14.0),
                            ]
                            .spacing(8.0)
                            .align_y(Vertical::Center),
                            text(crate::i18n::t!("package-remove-sandbox-warning")).size(12.0),
                            row![
                                iced::widget::space::horizontal(),
                                button(text(crate::i18n::t!("action-cancel")).size(12.0))
                                    .style(button_style::secondary)
                                    .on_press(Message::CancelTrust),
                                button(text(crate::i18n::t!("package-remove-sandbox")).size(12.0))
                                    .style(button_style::primary)
                                    .on_press(Message::SetTrusted(true)),
                            ]
                            .spacing(8.0)
                            .align_y(Vertical::Center),
                        ]
                        .spacing(10.0),
                    )
                    .padding(12.0)
                    .width(Length::Fill)
                    .style(common::banner_style),
                );
            } else {
                col = col.push(
                    row![
                        column![
                            text(crate::i18n::t!("package-remove-sandbox-advanced")).size(13.0),
                            text(crate::i18n::t!("package-remove-sandbox-help"))
                                .size(11.0)
                                .style(common::muted),
                        ]
                        .spacing(2.0),
                        iced::widget::space::horizontal(),
                        button(text(crate::i18n::t!("package-remove-sandbox-ellipsis")).size(12.0))
                            .style(button_style::secondary)
                            .on_press(Message::RequestTrust),
                    ]
                    .spacing(10.0)
                    .align_y(Vertical::Center),
                );
            }
        }
        col.into()
    }
}
