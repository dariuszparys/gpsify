//! Tauri v2 desktop runtime wiring for `gpsify-desktop`.

pub mod commands;
pub mod media_protocol;
pub mod state;

use tauri::menu::{Menu, PredefinedMenuItem, Submenu};
use tauri::Manager;

use crate::commands::*;
use crate::media_protocol::handle_media_request;
use crate::state::DesktopState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run(initial_handle: Option<String>) {
    let desktop_state = DesktopState::from_env(initial_handle).unwrap_or_else(|err| {
        eprintln!("failed to initialize gpsify configuration: {err}");
        std::process::exit(2);
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(desktop_state)
        .register_asynchronous_uri_scheme_protocol("gpsify", handle_media_request)
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                let handle = app.handle();
                let app_menu = Submenu::with_items(
                    handle,
                    "gpsify",
                    true,
                    &[
                        &PredefinedMenuItem::about(handle, Some("About gpsify"), None)?,
                        &PredefinedMenuItem::separator(handle)?,
                        &PredefinedMenuItem::services(handle, None)?,
                        &PredefinedMenuItem::separator(handle)?,
                        &PredefinedMenuItem::hide(handle, None)?,
                        &PredefinedMenuItem::hide_others(handle, None)?,
                        &PredefinedMenuItem::show_all(handle, None)?,
                        &PredefinedMenuItem::separator(handle)?,
                        &PredefinedMenuItem::quit(handle, None)?,
                    ],
                )?;
                let edit_menu = Submenu::with_items(
                    handle,
                    "Edit",
                    true,
                    &[
                        &PredefinedMenuItem::undo(handle, None)?,
                        &PredefinedMenuItem::redo(handle, None)?,
                        &PredefinedMenuItem::separator(handle)?,
                        &PredefinedMenuItem::cut(handle, None)?,
                        &PredefinedMenuItem::copy(handle, None)?,
                        &PredefinedMenuItem::paste(handle, None)?,
                        &PredefinedMenuItem::select_all(handle, None)?,
                    ],
                )?;
                let window_menu = Submenu::with_items(
                    handle,
                    "Window",
                    true,
                    &[
                        &PredefinedMenuItem::minimize(handle, None)?,
                        &PredefinedMenuItem::close_window(handle, None)?,
                    ],
                )?;
                let menu = Menu::with_items(handle, &[&app_menu, &edit_menu, &window_menu])?;
                app.set_menu(menu)?;
            }

            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            desktop_bootstrap,
            desktop_doctor,
            desktop_list_jobs,
            desktop_open_job,
            desktop_pick_folder,
            desktop_ingest,
            desktop_propose,
            desktop_geocode_pass,
            desktop_apply,
            desktop_undo,
            desktop_status,
            desktop_clean,
            desktop_skip_low,
            review_job,
            review_photos,
            review_photo_detail,
            review_cluster,
            review_decide,
            review_decide_many,
            review_apply_cluster,
            review_apply_pin,
            review_geocode,
        ])
        .run(tauri::generate_context!())
        .expect("error while running gpsify desktop application");
}
