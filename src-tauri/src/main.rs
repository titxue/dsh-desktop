// Prevents an additional console window on Windows in every build
// (debug builds spawn it unless the subsystem is overridden).
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    dsh_desktop_lib::run()
}
