use crate::utils::settings_ui::items::SettingsItem;
use winisland_core::config::{AppConfigField, MAX_HIDDEN_WIDTH};
use winisland_core::i18n::tr;

use super::super::{PageInput, SettingsPage};
use super::SettingsApp;

impl SettingsApp {
    pub(crate) fn build_behavior_page(&self) -> SettingsPage<()> {
        let mut page = SettingsPage::new();
        page.section(tr("section_auto_hide"));
        page.group_start();
        page.setting(&self.config, AppConfigField::AutoHide, true);
        if self.config.auto_hide {
            page.setting(&self.config, AppConfigField::AutoHideDelay, true);
        }
        page.setting(&self.config, AppConfigField::FullscreenAutoHide, true);
        if self.config.fullscreen_auto_hide {
            page.row_label(tr("fullscreen_auto_hide_note"));
        }
        page.push_setting(
            SettingsItem::RowStepper {
                label: tr("hidden_width"),
                value: if self.config.hidden_width >= MAX_HIDDEN_WIDTH {
                    tr("hidden_width_off")
                } else {
                    format!("{:.0}", self.config.hidden_width)
                },
                enabled: true,
            },
            AppConfigField::HiddenWidth,
        );
        page.group_end();

        page.section(tr("section_interaction"));
        page.group_start();
        page.setting(&self.config, AppConfigField::RightClickDrag, true);
        page.group_end();
        page
    }

    pub(crate) fn handle_behavior_click(&mut self, input: PageInput) {
        let page = self.build_behavior_page();
        let result = input.hit_test(&page);
        self.handle_setting_click(&page, &result, input);
    }
}
