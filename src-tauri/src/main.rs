#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let initial_handle = std::env::args().nth(1);
    gpsify_desktop_lib::run(initial_handle);
}
