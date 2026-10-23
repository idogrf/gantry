//! Delivering an alert outside the window: a native notification and a built-in sound.

use app_core::attention::Channels;
use app_core::settings::AlertTone;
use app_core::split_tree::NodeId;

/// A notification to show for one pane.
pub struct Notice {
    /// The pane's leaf (focused when the notification is clicked).
    pub leaf: NodeId,
    /// First line.
    pub summary: String,
    /// Details.
    pub body: String,
}

/// Show `notice` and play the sound as `channels` says, each on a thread of its own (both block). The sound plays at
/// `volume`. When the user clicks the notification, `notice.leaf` is sent to `clicked` (Linux and Windows; macOS
/// notifications have no click action here).
pub fn deliver(notice: Notice, channels: Channels, volume: f32, clicked: async_channel::Sender<NodeId>) {
    if let Some(tone) = channels.sound {
        play(tone, volume);
    }
    if channels.notification {
        std::thread::spawn(move || show(notice, clicked));
    }
}

fn show(notice: Notice, clicked: async_channel::Sender<NodeId>) {
    let mut notification = notify_rust::Notification::new();
    notification.appname("Gantry").summary(&notice.summary).body(&notice.body);
    // On Windows an action becomes a button; a click on the toast itself is the default response there.
    #[cfg(not(windows))]
    notification.action("default", "Show");
    let logo = logo_file();
    if let Some(path) = &logo {
        notification.image_path(path);
    }
    #[cfg(windows)]
    if windows_app_id::registered(logo.as_deref()) {
        notification.app_id(crate::APP_ID);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    notification.hint(notify_rust::Hint::DesktopEntry(crate::APP_ID.into()));
    let shown = notification.show();
    let handle = match shown {
        Ok(handle) => handle,
        Err(err) => {
            tracing::warn!("showing a notification failed: {err}");
            return;
        }
    };
    #[cfg(windows)]
    let _ = handle.wait_for_response(|response: &notify_rust::NotificationResponse| {
        if matches!(response, notify_rust::NotificationResponse::Default) {
            let _ = clicked.send_blocking(notice.leaf);
        }
    });
    #[cfg(all(unix, not(target_os = "macos")))]
    handle.wait_for_action(|action| {
        if action == "default" {
            let _ = clicked.send_blocking(notice.leaf);
        }
    });
    #[cfg(target_os = "macos")]
    let _ = (handle, clicked, notice.leaf);
}

/// Windows shows a toast under the app id it names once that id has a display name (and icon) in the registry;
/// without it the toast would be attributed to PowerShell.
#[cfg(windows)]
mod windows_app_id {
    use std::sync::OnceLock;

    /// Register Gantry's app id for this user (once per run, `icon` from the first call); whether that worked.
    pub fn registered(icon: Option<&str>) -> bool {
        static REGISTERED: OnceLock<bool> = OnceLock::new();
        *REGISTERED.get_or_init(|| {
            let key = format!(r"Software\Classes\AppUserModelId\{}", crate::APP_ID);
            let result =
                winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER).create_subkey(key).and_then(|(key, _)| {
                    key.set_value("DisplayName", &"Gantry")?;
                    match icon {
                        Some(icon) => key.set_value("IconUri", &icon),
                        None => Ok(()),
                    }
                });
            if let Err(err) = &result {
                tracing::warn!("registering the notification app id failed: {err}");
            }
            result.is_ok()
        })
    }
}

/// The app logo as a file in the temp dir (notifications take a path), written again when it is gone.
fn logo_file() -> Option<String> {
    let path = std::env::temp_dir().join("gantry-notification-logo.png");
    if !path.exists() {
        std::fs::write(&path, include_bytes!("../assets/icons/gantry.png")).ok()?;
    }
    path.to_str().map(str::to_string)
}

/// The embedded recording of `tone`.
fn recording(tone: AlertTone) -> &'static [u8] {
    match tone {
        AlertTone::ClassicBlip => include_bytes!("../assets/sounds/classic-blip.wav"),
        AlertTone::GentleRise => include_bytes!("../assets/sounds/gentle-rise.wav"),
        AlertTone::SoftBloop => include_bytes!("../assets/sounds/soft-bloop.wav"),
        AlertTone::SoftDrop => include_bytes!("../assets/sounds/soft-drop.wav"),
        AlertTone::FallingBlip => include_bytes!("../assets/sounds/falling-blip.wav"),
        AlertTone::SoftFall => include_bytes!("../assets/sounds/soft-fall.wav"),
        AlertTone::SoftFlutter => include_bytes!("../assets/sounds/soft-flutter.wav"),
        AlertTone::WarmHum => include_bytes!("../assets/sounds/warm-hum.wav"),
    }
}

/// Silence before every sound: a device that wakes late swallows this instead of the start of a short blip.
const LEAD_IN: std::time::Duration = std::time::Duration::from_millis(100);

/// Play `tone` at `volume` (0 to 1) on a thread of its own. Without an audio device it does nothing.
pub fn play(tone: AlertTone, volume: f32) {
    std::thread::spawn(move || {
        let mut sink = match rodio::DeviceSinkBuilder::open_default_sink() {
            Ok(sink) => sink,
            Err(err) => {
                tracing::debug!("no audio output for the alert sound: {err}");
                return;
            }
        };
        sink.log_on_drop(false);
        let source = match rodio::Decoder::new(std::io::Cursor::new(recording(tone))) {
            Ok(source) => source,
            Err(err) => {
                tracing::warn!("decoding the alert sound {tone:?} failed: {err}");
                return;
            }
        };
        let player = rodio::Player::connect_new(sink.mixer());
        player.set_volume(volume);
        player.append(rodio::Source::delay(source, LEAD_IN));
        player.sleep_until_end();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shows a real notification and plays every tone in turn. Run by hand:
    /// `cargo test -p ui-gpui alerts -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn a_notification_and_the_tones_reach_the_desktop() {
        for tone in AlertTone::ALL {
            play(tone, 0.4);
            std::thread::sleep(std::time::Duration::from_millis(900));
        }
        // Do not wait for a click: pass as soon as it is shown.
        let shown = notify_rust::Notification::new()
            .appname("Gantry")
            .summary("Gantry test")
            .body("Delivered by the alerts module test")
            .show();
        assert!(shown.is_ok(), "{:?}", shown.err());
    }

    #[test]
    fn every_tone_is_a_decodable_recording() {
        for tone in AlertTone::ALL {
            let source = rodio::Decoder::new(std::io::Cursor::new(recording(tone)))
                .unwrap_or_else(|err| panic!("{tone:?}: {err}"));
            assert!(source.count() > 0, "{tone:?}");
        }
    }
}
