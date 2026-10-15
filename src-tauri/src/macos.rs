//! macOS-only helpers (WP0).

/// Make this app the default handler for `magnet:` links via LaunchServices.
/// Only acts when running from a bundled .app (harmless no-op in dev).
#[cfg(target_os = "macos")]
pub fn register_default_magnet_handler(bundle_id: &str, data_dir: &std::path::Path) {
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        fn LSSetDefaultHandlerForURLScheme(scheme: CFStringRef, handler: CFStringRef) -> i32;
        fn LSCopyDefaultHandlerForURLScheme(scheme: CFStringRef) -> CFStringRef;
    }

    let in_bundle = std::env::current_exe()
        .map(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
        .unwrap_or(false);
    if !in_bundle {
        tracing::info!("not running from a .app bundle; skipping magnet: handler registration");
        return;
    }

    // Only claim magnet: once; if the user later picks another client, respect that.
    let flag = data_dir.join("magnet-handler-registered");
    if flag.exists() {
        return;
    }

    let scheme = CFString::new("magnet");
    let ours = CFString::new(bundle_id);

    // Skip if already the default (avoids any system prompt on every launch).
    let current = unsafe { LSCopyDefaultHandlerForURLScheme(scheme.as_concrete_TypeRef()) };
    if !current.is_null() {
        let current = unsafe { CFString::wrap_under_create_rule(current) }.to_string();
        if current.eq_ignore_ascii_case(bundle_id) {
            tracing::info!("already the default magnet: handler");
            let _ = std::fs::write(&flag, b"");
            return;
        }
        tracing::info!(%current, "current magnet: handler");
    }

    let status = unsafe {
        LSSetDefaultHandlerForURLScheme(scheme.as_concrete_TypeRef(), ours.as_concrete_TypeRef())
    };
    tracing::info!(status, "LSSetDefaultHandlerForURLScheme(magnet, {bundle_id})");
    if status == 0 {
        let _ = std::fs::write(&flag, b"");
    }
}

#[cfg(not(target_os = "macos"))]
pub fn register_default_magnet_handler(_bundle_id: &str, _data_dir: &std::path::Path) {}
