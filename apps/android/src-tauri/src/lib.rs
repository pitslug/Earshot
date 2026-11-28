//! Earshot on Android (M10): the Tauri 2 mobile shell. T10.2a only opens the window; the
//! recorder's commands come in T10.2b.

/// Starts the app: the entry point Android calls, and `main` on a desktop build.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if let Err(e) = tauri::Builder::default().run(tauri::generate_context!()) {
        // Nothing else can report it: the window never opened.
        eprintln!("earshot-android: the app could not start: {e}");
        std::process::exit(1);
    }
}
