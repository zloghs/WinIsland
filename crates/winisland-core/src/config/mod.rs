mod lyrics;
mod migrate;
mod visual;
mod widget_layout;
mod widgets;

use educe::Educe;
use serde::{Deserialize, Serialize};
use settings_schema::Settings;

pub use lyrics::*;
pub use migrate::*;
pub use visual::*;
pub use widget_layout::*;
pub use widgets::*;

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn existing_config_completed_onboarding() -> bool {
    true
}
pub const APP_AUTHOR: &str = "Eatgrapes";
pub const APP_HOMEPAGE: &str = "https://github.com/WinIslandProject/WinIsland";
pub const WINDOW_TITLE: &str = "WinIsland";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Educe, Settings)]
#[educe(Default)]
#[serde(default)]
pub struct AppConfig {
    #[serde(alias = "global_scale")]
    #[educe(Default = 1.0)]
    #[setting(number(min = 0.5, max = 5.0, step = 0.05, precision = 2))]
    pub compact_scale: f32,
    #[educe(Default = 1.0)]
    #[setting(number(min = 0.5, max = 5.0, step = 0.05, precision = 2))]
    pub expanded_scale: f32,
    #[educe(Default = 120.0)]
    #[setting(number(min = 40.0, max = 400.0, step = 5.0))]
    pub base_width: f32,
    #[educe(Default = 27.0)]
    #[setting(number(min = 15.0, max = 200.0, step = 2.0))]
    pub base_height: f32,
    #[educe(Default = 360.0)]
    #[setting(number(min = 200.0, max = 2000.0, step = 10.0))]
    pub expanded_width: f32,
    #[educe(Default = 200.0)]
    #[setting(number(min = 100.0, max = 1000.0, step = 10.0))]
    pub expanded_height: f32,
    #[educe(Default = true)]
    #[setting(toggle)]
    pub motion_blur: bool,
    #[educe(Default = "default")]
    pub island_style: String,
    #[educe(Default = 6.0)]
    #[setting(number(min = 0.0, max = 30.0, step = 1.0))]
    pub liquid_glass_blur: f32,
    #[educe(Default = 200.0)]
    #[setting(number(min = 0.0, max = 400.0, step = 10.0))]
    pub liquid_glass_refraction: f32,
    #[educe(Default = 40.0)]
    #[setting(number(min = 0.0, max = 100.0, step = 5.0))]
    pub liquid_glass_dispersion: f32,
    #[educe(Default = 100.0)]
    #[setting(number(min = 0.0, max = 200.0, step = 10.0))]
    pub liquid_glass_tint: f32,
    #[setting(toggle)]
    pub liquid_glass_siri: bool,
    #[setting(toggle)]
    pub liquid_glass_compat_capture: bool,
    #[educe(Default = "forever")]
    #[setting(choice(
        "forever" => "lingosoul_history_forever",
        "7" => "lingosoul_history_7d",
        "30" => "lingosoul_history_30d",
        "90" => "lingosoul_history_90d",
    ))]
    pub lingosoul_history_retention: String,
    /// Start the installed LingoSoul agent together with the island.
    #[setting(toggle)]
    pub lingosoul_autostart: bool,
    /// Install new LingoSoul releases automatically while it is idle.
    #[educe(Default = true)]
    #[setting(toggle)]
    pub lingosoul_auto_update: bool,
    #[setting(number(min = 0.0, max = 50.0, step = 1.0))]
    pub glass_blur: f32,
    #[educe(Default = true)]
    #[setting(toggle, label = "smtc_control")]
    pub smtc_enabled: bool,
    pub music_notice_acknowledged: bool,
    #[serde(default = "existing_config_completed_onboarding")]
    pub onboarding_completed: bool,
    pub smtc_apps: Vec<String>,
    pub smtc_known_apps: Vec<String>,
    #[educe(Default = true)]
    #[setting(toggle)]
    pub show_lyrics: bool,
    #[setting(toggle)]
    pub show_secondary_lyrics: bool,
    #[educe(Default = "online")]
    #[setting(choice("online" => "lyrics_mode_online", "lrc" => "lyrics_mode_lrc"))]
    pub lyrics_mode: String,
    pub lyrics_local_dir: Option<String>,
    pub custom_font_path: Option<String>,
    pub font_family: Option<String>,
    #[educe(Default = "regular")]
    #[setting(choice(
        "light" => "font_weight_light",
        "regular" => "font_weight_regular",
        "bold" => "font_weight_bold",
    ))]
    pub font_weight: String,
    pub auto_start: bool,
    #[setting(toggle)]
    pub auto_hide: bool,
    #[setting(toggle)]
    pub fullscreen_auto_hide: bool,
    #[educe(Default = 5.0)]
    #[setting(number(min = 1.0, max = 60.0, step = 1.0), label = "hide_delay")]
    pub auto_hide_delay: f32,
    #[educe(Default = 5.0)]
    #[setting(number(min = MIN_HIDDEN_WIDTH, max = MAX_HIDDEN_WIDTH, step = 1.0))]
    pub hidden_width: f32,
    #[educe(Default = true)]
    #[setting(toggle, label = "check_updates")]
    pub check_for_updates: bool,
    #[educe(Default = 4.0)]
    #[setting(number(min = 1.0, max = 24.0, step = 1.0), label = "update_interval")]
    pub update_check_interval: f32,
    #[educe(Default = "auto")]
    pub language: String,
    #[educe(Default = "163")]
    #[setting(choice(
        "163" => "lyrics_source_163",
        "qq" => "lyrics_source_qq",
        "kugou" => "lyrics_source_kugou",
        "lrclib" => "lyrics_source_lrclib",
        "amll" => "AMLL",
        "lxmusic" => "LX Music",
    ))]
    pub lyrics_source: String,
    #[setting(number(min = -10.0, max = 10.0, step = 0.1, precision = 1))]
    pub lyrics_delay: f64,
    #[setting(toggle)]
    pub lyrics_scroll: bool,
    #[educe(Default = 300.0)]
    #[setting(number(min = 100.0, max = 500.0, step = 10.0))]
    pub lyrics_scroll_max_width: f32,
    #[educe(Default = 6.0)]
    #[setting(number(min = 0.0, max = 32.0, step = 1.0))]
    pub lyrics_side_gap: f32,
    #[setting(choice(
        "random" => "lyrics_transition_random",
        "blur" => "lyrics_transition_blur",
        "slide" => "lyrics_transition_slide",
        "fade" => "lyrics_transition_fade",
    ))]
    pub lyrics_transition_animation: LyricTransitionMode,
    #[setting(number(step = 5.0))]
    pub position_x_offset: i32,
    #[setting(number(step = 5.0))]
    pub position_y_offset: i32,
    #[serde(rename = "dock_position", skip_serializing_if = "Option::is_none")]
    pub legacy_dock_position: Option<DockPosition>,
    pub monitor_index: i32,
    #[setting(number(min = 0.0, max = 30.0, step = 1.0))]
    pub font_size: f32,
    #[setting(number(min = -32.0, max = 32.0, step = 1.0))]
    pub music_text_y_offset: f32,
    #[educe(Default = 22.0)]
    #[setting(number(min = 8.0, max = 64.0, step = 1.0))]
    pub music_text_line_spacing: f32,
    #[educe(Default = "system")]
    #[setting(choice("system" => "theme_system", "light" => "theme_light", "dark" => "theme_dark"))]
    pub settings_theme: String,
    #[educe(Default = "square")]
    pub mini_cover_shape: String,
    #[educe(Default = "square")]
    pub expanded_cover_shape: String,
    pub cover_rotate: bool,
    #[educe(Default = "stable")]
    #[setting(choice("stable" => "channel_stable", "beta" => "channel_beta"))]
    pub update_channel: String,
    #[setting(toggle)]
    pub right_click_drag: bool,
    #[setting(toggle)]
    pub notification_display: bool,
    #[setting(toggle)]
    pub device_status_indicators: bool,
    #[educe(Default = true)]
    #[setting(toggle)]
    pub system_controls_overlay: bool,
    #[educe(Default = true)]
    #[setting(toggle, label = "clipboard_link_prompt")]
    pub clipboard_link_prompt: bool,
    #[educe(Default(expression = default_expanded_page_order()))]
    pub expanded_page_order: Vec<ExpandedPageKind>,
    pub hidden_expanded_pages: Vec<ExpandedPageKind>,
    pub plugin_pages: Vec<PluginPageSlot>,
    #[educe(Default(expression = default_widget_layout()))]
    pub widget_layout: Vec<WidgetSlot>,
    pub plugin_widget_layout: Vec<PluginWidgetSlot>,
    pub compact_widget_layout: Vec<CompactWidgetSlot>,
    #[educe(Default(expression = default_resource_metrics()))]
    pub resource_metrics: Vec<ResourceMetricConfig>,
    #[educe(Default(expression = default_resource_metrics()))]
    pub compact_resource_metrics: Vec<ResourceMetricConfig>,
    #[educe(Default = 2)]
    pub resource_widget_columns: usize,
    #[educe(Default = 1)]
    pub resource_widget_rows: usize,
    #[serde(default)]
    #[educe(Default(expression = CONFIG_VERSION))]
    pub config_version: u32,
}
