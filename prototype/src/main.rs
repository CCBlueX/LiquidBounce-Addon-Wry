mod common;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod nested;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "offscreen".into());

    #[cfg(target_os = "linux")]
    linux::run(&mode);
    #[cfg(target_os = "macos")]
    macos::run(&mode);
    #[cfg(target_os = "windows")]
    windows::run(&mode);
}
