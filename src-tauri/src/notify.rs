//! Native macOS notifications via `osascript` (no extra crate; title/body are passed
//! as argv, so no AppleScript quoting is involved). Setting `notifications_enabled`
//! lives in `data_dir/settings.json` (see `netmode::with_settings`).

use serde::{Deserialize, Serialize};

use crate::netmode::{update_settings, with_settings};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotifySettings {
    pub enabled: bool,
}

const SCRIPT: [&str; 3] = [
    "on run argv",
    "display notification (item 2 of argv) with title (item 1 of argv)",
    "end run",
];

/// Argument list for `/usr/bin/osascript` (pure, unit-tested).
fn osascript_args(title: &str, body: &str) -> Vec<String> {
    let mut v = Vec::new();
    for line in SCRIPT {
        v.push("-e".to_string());
        v.push(line.to_string());
    }
    // A leading '-' would not be an option here (after the script), but keep
    // control chars out of the banner.
    let clean = |s: &str| s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>();
    v.push(clean(title));
    v.push(clean(body));
    v
}

/// Show a notification (fire-and-forget; no-op when disabled in settings).
pub fn notify(title: &str, body: &str) {
    if !with_settings(|s| s.notifications_enabled) {
        return;
    }
    let args = osascript_args(title, body);
    std::thread::spawn(move || {
        match std::process::Command::new("/usr/bin/osascript")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(s) if s.success() => {}
            Ok(s) => tracing::debug!("osascript notification exited with {s}"),
            Err(e) => tracing::debug!("osascript notification failed: {e}"),
        }
    });
}

#[tauri::command]
pub fn notify_get_settings() -> Result<NotifySettings, String> {
    Ok(NotifySettings { enabled: with_settings(|s| s.notifications_enabled) })
}

/// JS: `{ enabled }`.
#[tauri::command]
pub fn notify_set_settings(enabled: bool) -> Result<NotifySettings, String> {
    update_settings(|s| s.notifications_enabled = enabled)?;
    Ok(NotifySettings { enabled })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_pass_text_verbatim() {
        let a = osascript_args("Done \"x\" \\ 'y'", "Line1\nLine2");
        assert_eq!(a.len(), 8);
        assert_eq!(a[0], "-e");
        assert_eq!(a[6], "Done \"x\" \\ 'y'");
        assert_eq!(a[7], "Line1 Line2");
    }
}
