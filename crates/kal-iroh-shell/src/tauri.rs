//! Execute [`ShellCommand`]s on a Tauri webview window.

use crate::ShellCommand;

/// Run `rx`'s commands on the webview window labelled `label`, until the
/// channel closes.  Spawn it on Tauri's async runtime.
pub async fn run_shell_commands<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    label: &str,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ShellCommand>,
) {
    use tauri::Manager;
    while let Some(cmd) = rx.recv().await {
        let Some(window) = app.get_webview_window(label) else {
            log::warn!("kal-iroh-shell: no window {label:?} for {cmd:?}");
            continue;
        };
        let result = match cmd {
            ShellCommand::Eval(js) => window.eval(js),
            ShellCommand::Reload => window.reload(),
        };
        if let Err(e) = result {
            log::warn!("kal-iroh-shell: shell command failed: {e}");
        }
    }
}
