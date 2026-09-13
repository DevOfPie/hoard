//! Miscellaneous commands used by the dev scaffolding.

/// Round-trips a name through the Rust backend so the UI can prove the
/// `invoke()` plumbing works end-to-end.
#[tauri::command]
pub fn greet(name: &str) -> String {
    format!("Hello from Rust, {name}! 🪙")
}

/// Open a web URL in the user's default browser with a **sanitized** child
/// environment. Replaces the frontend `@tauri-apps/plugin-shell` `open` for
/// every outward link (OAuth sign-in, upgrade/billing pages, terms).
///
/// Why not just use the plugin: inside an AppImage, `AppRun` exports
/// `LD_LIBRARY_PATH` / `LD_PRELOAD` / `GTK_PATH` / … pointing at Hoard's bundled
/// libraries. A browser spawned via the plugin inherits them and loads our
/// (version-mismatched) Wayland/EGL libs instead of the host's; on SteamOS and
/// Bazzite it then dies before drawing a window, so the "Sign in" button opened
/// *nothing* even though the loopback listener was already up. We strip those
/// vars (restoring `*_ORIG` if AppRun saved them) so the browser starts against
/// the system libraries. On macOS/Windows there's no such pollution; we just
/// hand the URL to the platform opener.
#[tauri::command]
pub async fn open_external(url: String) -> Result<(), String> {
    // Never feed an arbitrary string to a shell or opener: web schemes only.
    let allowed =
        url.starts_with("https://") || url.starts_with("http://") || url.starts_with("mailto:");
    if !allowed {
        return Err("refusing to open non-web URL".into());
    }
    platform_opener(&url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open browser: {e}"))
}

/// Shows a folder in the file manager. The one caller is the bell's "Open side
/// copy", whose path the service wrote (`SaveConflictsBackedUp::conflict_dir`);
/// anything that is not an existing directory is refused, so a string that is
/// not a folder never reaches the opener.
#[tauri::command]
pub async fn open_folder(path: String) -> Result<(), String> {
    let dir = std::path::Path::new(&path);
    if !dir.is_absolute() || !dir.is_dir() {
        return Err("refusing to open something that isn't a folder".into());
    }
    platform_opener(&path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open the folder: {e}"))
}

/// The platform's "open this" command with `arg`, not yet spawned. Web URLs
/// and folders take the same road.
fn platform_opener(arg: &str) -> tokio::process::Command {
    let url = arg;
    use tokio::process::Command;

    #[cfg(target_os = "linux")]
    let cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        // AppImage-injected loader/toolkit vars: restore the pre-AppImage value
        // if AppRun stashed it as `<VAR>_ORIG`, otherwise drop it entirely so
        // the child falls back to the host defaults.
        const POLLUTED: &[&str] = &[
            "LD_LIBRARY_PATH",
            "LD_PRELOAD",
            "GTK_PATH",
            "GDK_PIXBUF_MODULE_FILE",
            "GIO_MODULE_DIR",
            "GST_PLUGIN_SYSTEM_PATH",
            "GSETTINGS_SCHEMA_DIR",
        ];
        for var in POLLUTED {
            match std::env::var_os(format!("{var}_ORIG")) {
                Some(orig) => {
                    c.env(var, orig);
                }
                None => {
                    c.env_remove(var);
                }
            }
        }
        c
    };

    #[cfg(target_os = "macos")]
    let cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };

    #[cfg(target_os = "windows")]
    let cmd = {
        // Do NOT route through `cmd /C start`: cmd re-parses its command line
        // and treats every `&` in the URL as a command separator, so an OAuth
        // sign-in URL like `.../login?desktop=1&port=65491&state=<nonce>` was
        // truncated at the first `&`, so the browser only ever received
        // `?desktop=1`, dropping the loopback port and the CSRF nonce. The
        // callback then reached the app with no `state`, and every desktop
        // sign-in failed with "auth callback state mismatch". rundll32 is not a
        // shell: it hands the URL to the registered protocol handler verbatim.
        let mut c = Command::new("rundll32.exe");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };

    cmd
}

/// One line from the interface into the app's log, for what only the webview can
/// see: today, which display language it picked and why. Capped, so a runaway
/// caller can't flood the file.
#[tauri::command]
pub fn ui_log(window: tauri::Window, topic: String, message: String) {
    let topic: String = topic.chars().take(32).collect();
    let message: String = message.chars().take(400).collect();
    tracing::info!(window = window.label(), topic = %topic, "ui: {message}");
}
