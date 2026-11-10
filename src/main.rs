#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod audio;
mod config;
mod runtime;
mod ui;

fn main() {
	ui::run();
}
