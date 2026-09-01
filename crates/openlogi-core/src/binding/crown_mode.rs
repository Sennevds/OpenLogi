//! Crown modes: an ordered list of what the dial does, cycled by a tap.
//!
//! A Craft's crown has one physical rotation axis but many plausible jobs —
//! volume, scroll, zoom, tab switching. A mode bundles the actions for one of
//! those jobs; [`Action::CycleCrownMode`] advances to the next. This is the
//! config half: the ordered list per device. The active index is runtime state
//! held by the agent, deliberately not persisted (see `openlogi-agent-core`).
//!
//! Config-file only — never crosses the IPC, so it is free to evolve without a
//! `PROTOCOL_VERSION` bump. (`Action::CycleCrownMode` *is* a wire type; the
//! mode list itself reaches the agent through a config reload.)

use serde::{Deserialize, Serialize};

use super::action::Action;

/// One entry in a device's crown-mode list.
///
/// Every control is optional and an unset one falls through to that control's
/// ordinary binding, so a mode can redefine rotation while leaving the press
/// alone. The tap is deliberately absent: it is what cycles modes, and a mode
/// that could rebind it could strand the user inside itself.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrownMode {
    /// Name shown by the mode indicator and the GUI. Free text: it is the
    /// user's vocabulary for their own workflow, not a catalog key.
    pub name: String,
    /// Action for one detent of upward rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotate_up: Option<Action>,
    /// Action for one detent of downward rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotate_down: Option<Action>,
    /// Action for a press of the crown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub press: Option<Action>,
}

impl CrownMode {
    /// The action this mode assigns to `control`, or `None` when the mode
    /// leaves it to the control's ordinary binding.
    ///
    /// Returns `None` for any control a mode cannot own — the tap above all,
    /// so cycling always works no matter which mode is active.
    #[must_use]
    pub fn action_for(&self, control: super::ButtonId) -> Option<&Action> {
        use super::ButtonId as B;
        match control {
            B::CrownRotateUp => self.rotate_up.as_ref(),
            B::CrownRotateDown => self.rotate_down.as_ref(),
            B::CrownPress => self.press.as_ref(),
            _ => None,
        }
    }

    /// Whether this mode assigns anything at all. An empty mode is legal but
    /// inert — every control falls through — so callers can skip it rather
    /// than cycle into a mode that does nothing.
    #[must_use]
    pub fn is_inert(&self) -> bool {
        self.rotate_up.is_none() && self.rotate_down.is_none() && self.press.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::ButtonId;

    fn volume() -> CrownMode {
        CrownMode {
            name: "Volume".into(),
            rotate_up: Some(Action::VolumeUp),
            rotate_down: Some(Action::VolumeDown),
            press: None,
        }
    }

    #[test]
    fn a_mode_answers_only_for_the_controls_it_sets() {
        let mode = volume();
        assert_eq!(
            mode.action_for(ButtonId::CrownRotateUp),
            Some(&Action::VolumeUp)
        );
        assert_eq!(
            mode.action_for(ButtonId::CrownRotateDown),
            Some(&Action::VolumeDown)
        );
        // Unset: the press keeps whatever its ordinary binding is.
        assert_eq!(mode.action_for(ButtonId::CrownPress), None);
    }

    /// A mode must never own the tap, or a mode whose tap did something else
    /// would trap the user with no way to cycle out.
    #[test]
    fn a_mode_never_owns_the_tap() {
        let mode = CrownMode {
            name: "Trap".into(),
            rotate_up: Some(Action::VolumeUp),
            rotate_down: None,
            press: Some(Action::PlayPause),
        };
        assert_eq!(mode.action_for(ButtonId::CrownTap), None);
    }

    #[test]
    fn a_mode_that_sets_nothing_is_inert() {
        assert!(CrownMode::default().is_inert());
        assert!(!volume().is_inert());
    }

    /// Non-crown buttons are not a mode's business even if one is passed in.
    #[test]
    fn an_unrelated_button_resolves_to_nothing() {
        assert_eq!(volume().action_for(ButtonId::MiddleClick), None);
    }
}
