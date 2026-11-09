//! Files the interface draws from, embedded in the binary.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// The dev container icon (a box), drawn by `theme::container_icon`.
pub(crate) const CONTAINER_ICON: &str = "icons/container.svg";

/// The SSH host icon (two stacked servers), drawn by `theme::ssh_icon`.
pub(crate) const SSH_ICON: &str = "icons/ssh.svg";

/// The folder browser's folder icon, drawn by `launcher_view`.
pub(crate) const FOLDER_ICON: &str = "icons/folder.svg";

/// The split right button's icon (a window with its right half filled).
pub(crate) const SPLIT_RIGHT_ICON: &str = "icons/split-right.svg";

/// The split down button's icon (a window with its bottom half filled).
pub(crate) const SPLIT_DOWN_ICON: &str = "icons/split-down.svg";

/// The Gantry project's icon (three stacked layers), drawn by `sidebar_view`, `launcher_view` and `recents_view`.
pub(crate) const LAYERS_ICON: &str = "icons/layers.svg";

/// The close pane button's icon (an X).
pub(crate) const CLOSE_ICON: &str = "icons/close.svg";

/// The jump to bottom chip's icon (an arrow pointing down), drawn by `terminal_view`.
pub(crate) const ARROW_DOWN_ICON: &str = "icons/arrow-down.svg";

/// The project heading's fold chevron when it is open (pointing down), drawn by `sidebar_view`.
pub(crate) const CHEVRON_DOWN_ICON: &str = "icons/chevron-down.svg";

/// The project heading's fold chevron when it is folded (pointing right), drawn by `sidebar_view`.
pub(crate) const CHEVRON_RIGHT_ICON: &str = "icons/chevron-right.svg";

/// The last prompt card's icon (a chat bubble), drawn by `workspace_view::tab_content::prompt_card`.
pub(crate) const PROMPT_ICON: &str = "icons/prompt.svg";

/// The pin on a row marked unread (a bookmark), drawn by `theme::pin_icon`.
pub(crate) const PIN_ICON: &str = "icons/pin.svg";

/// The git chip's icon (a branch), drawn by `git_chip`.
pub(crate) const BRANCH_ICON: &str = "icons/branch.svg";

/// The sidebar toggle tab's collapse arrow, drawn by `workspace_view` (the expand arrow is `CHEVRON_RIGHT_ICON`).
pub(crate) const CHEVRON_LEFT_ICON: &str = "icons/chevron-left.svg";

/// The recent sessions pop-up's reset filters button (a counter-clockwise arrow), drawn by `recents_view`.
pub(crate) const RESET_ICON: &str = "icons/reset.svg";

/// The app icon in color, drawn next to the name in the title bar (a picture, not a tinted icon like the others).
pub(crate) const LOGO: &str = "icons/gantry.png";

/// Gantry's embedded assets.
pub(crate) struct Assets;

/// The guide link's icon (an i in a circle), drawn by `settings_view::guide_link`.
pub(crate) const INFO_ICON: &str = "icons/info.svg";

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            CONTAINER_ICON => Some(include_bytes!("../assets/icons/container.svg")),
            SSH_ICON => Some(include_bytes!("../assets/icons/ssh.svg")),
            FOLDER_ICON => Some(include_bytes!("../assets/icons/folder.svg")),
            SPLIT_RIGHT_ICON => Some(include_bytes!("../assets/icons/split-right.svg")),
            SPLIT_DOWN_ICON => Some(include_bytes!("../assets/icons/split-down.svg")),
            CLOSE_ICON => Some(include_bytes!("../assets/icons/close.svg")),
            LAYERS_ICON => Some(include_bytes!("../assets/icons/layers.svg")),
            ARROW_DOWN_ICON => Some(include_bytes!("../assets/icons/arrow-down.svg")),
            CHEVRON_DOWN_ICON => Some(include_bytes!("../assets/icons/chevron-down.svg")),
            CHEVRON_RIGHT_ICON => Some(include_bytes!("../assets/icons/chevron-right.svg")),
            PROMPT_ICON => Some(include_bytes!("../assets/icons/prompt.svg")),
            PIN_ICON => Some(include_bytes!("../assets/icons/pin.svg")),
            INFO_ICON => Some(include_bytes!("../assets/icons/info.svg")),
            BRANCH_ICON => Some(include_bytes!("../assets/icons/branch.svg")),
            CHEVRON_LEFT_ICON => Some(include_bytes!("../assets/icons/chevron-left.svg")),
            RESET_ICON => Some(include_bytes!("../assets/icons/reset.svg")),
            LOGO => Some(include_bytes!("../assets/icons/gantry.png")),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(vec![
            CONTAINER_ICON.into(),
            SSH_ICON.into(),
            SPLIT_RIGHT_ICON.into(),
            SPLIT_DOWN_ICON.into(),
            CLOSE_ICON.into(),
            LAYERS_ICON.into(),
            ARROW_DOWN_ICON.into(),
            CHEVRON_DOWN_ICON.into(),
            CHEVRON_RIGHT_ICON.into(),
            PROMPT_ICON.into(),
            PIN_ICON.into(),
            INFO_ICON.into(),
            CHEVRON_LEFT_ICON.into(),
            RESET_ICON.into(),
            LOGO.into(),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icons_are_embedded_and_nothing_else_is() {
        for icon in [
            CONTAINER_ICON,
            SSH_ICON,
            SPLIT_RIGHT_ICON,
            SPLIT_DOWN_ICON,
            CLOSE_ICON,
            LAYERS_ICON,
            ARROW_DOWN_ICON,
            CHEVRON_DOWN_ICON,
            CHEVRON_RIGHT_ICON,
            PROMPT_ICON,
            PIN_ICON,
            INFO_ICON,
            RESET_ICON,
        ] {
            let bytes = Assets.load(icon).unwrap().expect("embedded");
            assert!(std::str::from_utf8(&bytes).unwrap().starts_with("<svg"), "{icon}");
        }
        assert!(Assets.load(LOGO).unwrap().expect("embedded").starts_with(b"\x89PNG"));
        assert!(Assets.load("icons/missing.svg").unwrap().is_none());
    }
}
