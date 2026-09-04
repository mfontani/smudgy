//! Local-package Manifest-tab adapter.

use super::*;

impl AutomationsWindow {
    pub(super) fn view_package_manifest_tab(&self) -> Elem<'_> {
        self.view_manifest_section()
    }
}
