use std::borrow::Cow;
use std::rc::Rc;

use gpui_kit::component::{Theme, ThemeMode, ThemeSet};
use gpui_kit::{App, Hsla, Window, rgb};

use crate::store::Appearance;

const THEME_JSON: &str = include_str!("../assets/themes/duckplus.json");

/// JetBrains Mono (SIL OFL 1.1, see assets/fonts/JetBrainsMono-OFL.txt),
/// embedded so the editor and grid look identical on every machine.
const FONTS: [&[u8]; 5] = [
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Italic.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-SemiBold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf"),
];

/// Tag colors for connections (like TablePlus' environment colors).
pub const TAG_COLORS: [(u32, &str); 6] = [
    (0xffd43b, "Duck"),
    (0x3ecf8e, "Local"),
    (0x5aa7ff, "Dev"),
    (0xc792ea, "Staging"),
    (0xff9e64, "Testing"),
    (0xf2555a, "Production"),
];

pub fn tag_color(ix: usize) -> Hsla {
    rgb(TAG_COLORS[ix % TAG_COLORS.len()].0).into()
}

pub fn init(cx: &mut App) {
    if let Err(e) = cx
        .text_system()
        .add_fonts(FONTS.iter().map(|f| Cow::Borrowed(*f)).collect())
    {
        eprintln!("duckplus: failed to load bundled fonts: {e:#}");
    }
    let set: ThemeSet = serde_json::from_str(THEME_JSON).expect("bundled theme is valid");
    let theme = Theme::global_mut(cx);
    for config in set.themes {
        let config = Rc::new(config);
        if config.mode.is_dark() {
            theme.dark_theme = config;
        } else {
            theme.light_theme = config;
        }
    }
}

/// Apply the user's appearance preference; `System` follows the OS.
pub fn apply(appearance: Appearance, window: Option<&mut Window>, cx: &mut App) {
    let mode = match appearance {
        Appearance::Light => ThemeMode::Light,
        Appearance::Dark => ThemeMode::Dark,
        Appearance::System => match cx.window_appearance() {
            gpui_kit::WindowAppearance::Dark | gpui_kit::WindowAppearance::VibrantDark => {
                ThemeMode::Dark
            }
            _ => ThemeMode::Light,
        },
    };
    Theme::change(mode, window, cx);
}
