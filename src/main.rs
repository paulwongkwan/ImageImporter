// Release builds are GUI-only: no console window should flash up on launch.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gpui_kit::component::Theme;
use gpui_kit::*;
use image_importer::settings::Settings;
use image_importer::ui_gpui::{self, ImporterWindow};

fn main() {
    application()
        .with_assets(assets::Assets)
        .run(|cx| {
            init(cx);
            ui_gpui::bind_keys(cx);

            let settings = Settings::load();
            let (width, height) = settings.usable_window_size();
            let (x, y) = settings.usable_window_position().unwrap_or((120.0, 90.0));

            let mut options = WindowOptions::default();
            options.titlebar = Some(TitlebarOptions {
                title: Some("Image Importer".into()),
                ..Default::default()
            });
            let bounds = Bounds::new(point(px(x), px(y)), size(px(width), px(height)));
            options.window_bounds = Some(if settings.maximized {
                WindowBounds::Maximized(bounds)
            } else {
                WindowBounds::Windowed(bounds)
            });

            open_window(options, cx, |window, cx| {
                // Follow the Windows light or dark setting, so the drawn title
                // bar and the body always agree.
                Theme::sync_system_appearance(Some(&mut *window), cx);
                cx.new(|cx| ImporterWindow::new(window, cx))
            })
            .expect("Failed to open window");
        });
}
