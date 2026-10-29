#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
fn main() {
    let Some(plan) = std::env::args_os().nth(1) else {
        return;
    };
    if let Err(error) = azs_uploader::recovery::run_guard(std::path::Path::new(&plan)) {
        azs_uploader::recovery::record_guard_error(std::path::Path::new(&plan), &error);
        // The preserved package and plan remain available for an explicit repair.
        eprintln!("Update recovery failed: {error:#}");
    }
}
