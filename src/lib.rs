//! Shared library for `monitor-toggled` (daemon) and `monitor-toggle` (CLI).

pub mod autostart;
pub mod config;
pub mod controller;
pub mod display;
pub mod hotkey;
pub mod input_guard;
pub mod ipc;
pub mod launcher;
pub mod logging;
pub mod tray;
pub mod web_server;
pub mod win_util;
pub mod window_restore;
