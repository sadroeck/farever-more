#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod archive;
mod backend;
mod config;
mod file_dialog;
mod model;

slint::include_modules!();

fn main() {
    if let Err(error) = app::run() {
        eprintln!("farever-more-manager: {error}");
        std::process::exit(1);
    }
}
