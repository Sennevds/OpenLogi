//! Which crown mode each device is currently in.
//!
//! The mode *list* is config ([`CrownMode`]); the active position is runtime
//! state, held here and mirroring [`DpiCycles`]: one entry per device, keyed by
//! config key, advanced by an action.
//!
//! Deliberately not persisted. A mode is a transient "what is the dial doing
//! right now" answer, and restoring a stale one across an agent restart would
//! leave the dial doing something the user last chose hours ago with no
//! on-screen reminder. Every device therefore starts on its first mode.
//!
//! [`DpiCycles`]: crate::DpiCycles

use std::collections::HashMap;

use openlogi_core::binding::{Action, ButtonId, CrownMode};

/// Per-device crown-mode positions.
#[derive(Debug, Clone, Default)]
pub struct CrownModes {
    by_key: HashMap<String, CrownModeState>,
}

impl CrownModes {
    /// Replace every device's mode list, preserving each one's position where
    /// the list it was cycling through is unchanged.
    ///
    /// A config reload rebuilds this wholesale, and resetting to the first mode
    /// on every unrelated config edit would yank the dial out from under the
    /// user. A device whose list actually changed does reset: its old index no
    /// longer means anything.
    pub fn republish(&mut self, lists: HashMap<String, Vec<CrownMode>>) {
        let mut next: HashMap<String, CrownModeState> = HashMap::with_capacity(lists.len());
        for (key, modes) in lists {
            let index = self
                .by_key
                .get(&key)
                .filter(|state| state.modes == modes)
                .map_or(0, |state| state.index);
            next.insert(key, CrownModeState { modes, index });
        }
        self.by_key = next;
    }

    /// The state for `key`, or `None` when that device has no modes.
    pub fn state_for(&mut self, key: Option<&str>) -> Option<&mut CrownModeState> {
        self.by_key.get_mut(key?)
    }

    /// The action `control` should run on `key` right now, or `None` to fall
    /// through to the control's ordinary binding.
    #[must_use]
    pub fn action_for(&self, key: Option<&str>, control: ButtonId) -> Option<&Action> {
        self.by_key.get(key?)?.action_for(control)
    }

    /// The active mode's name on `key`, for logging and the indicator.
    #[must_use]
    pub fn active_name(&self, key: Option<&str>) -> Option<&str> {
        self.by_key
            .get(key?)?
            .active()
            .map(|mode| mode.name.as_str())
    }
}

/// What an on-screen indicator needs to draw the mode list: every mode's name
/// in cycle order, and which one is active.
///
/// Deliberately plain data with no platform types: the agent-core half decides
/// *that* the indicator should update, and each platform's own presenter
/// decides how (or whether) to draw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrownModeIndicator {
    /// Mode names in cycle order.
    pub names: Vec<String>,
    /// Index into [`Self::names`] of the active mode.
    pub selected: usize,
}

/// One device's mode list plus where in it the dial currently sits.
#[derive(Debug, Clone, Default)]
pub struct CrownModeState {
    modes: Vec<CrownMode>,
    index: usize,
}

impl CrownModeState {
    /// Build a state positioned at the first mode.
    #[must_use]
    pub fn new(modes: Vec<CrownMode>) -> Self {
        Self { modes, index: 0 }
    }

    /// Advance to the next mode (wrapping last to first) and return it.
    ///
    /// Inert modes are skipped: cycling into a mode that assigns nothing would
    /// look like the tap had failed. `None` when no mode is usable at all,
    /// which keeps the action a no-op rather than a silent index change.
    pub fn cycle(&mut self) -> Option<&CrownMode> {
        if self.modes.iter().all(CrownMode::is_inert) {
            return None;
        }
        // Bounded by one full lap; the guard above proves it finds a usable
        // mode before the lap runs out.
        for _ in 0..self.modes.len() {
            self.index = (self.index + 1) % self.modes.len();
            if !self.modes[self.index].is_inert() {
                break;
            }
        }
        self.modes.get(self.index)
    }

    /// The mode the dial is in, if the list is non-empty.
    #[must_use]
    pub fn active(&self) -> Option<&CrownMode> {
        self.modes.get(self.index)
    }

    /// The active mode's action for `control`, or `None` to fall through.
    #[must_use]
    pub fn action_for(&self, control: ButtonId) -> Option<&Action> {
        self.active()?.action_for(control)
    }

    /// Snapshot for an on-screen indicator. `None` when there is nothing worth
    /// showing, so a presenter never has to decide that for itself.
    #[must_use]
    pub fn indicator(&self) -> Option<CrownModeIndicator> {
        if self.modes.is_empty() {
            return None;
        }
        Some(CrownModeIndicator {
            names: self.modes.iter().map(|mode| mode.name.clone()).collect(),
            selected: self.index,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(name: &str, up: Action) -> CrownMode {
        CrownMode {
            name: name.into(),
            rotate_up: Some(up),
            rotate_down: None,
            press: None,
        }
    }

    fn inert(name: &str) -> CrownMode {
        CrownMode {
            name: name.into(),
            ..CrownMode::default()
        }
    }

    #[test]
    fn cycling_wraps_and_reports_the_new_mode() {
        let mut state = CrownModeState::new(vec![
            mode("Volume", Action::VolumeUp),
            mode("Scroll", Action::ScrollUp),
        ]);
        assert_eq!(state.active().map(|m| m.name.as_str()), Some("Volume"));
        assert_eq!(state.cycle().map(|m| m.name.as_str()), Some("Scroll"));
        assert_eq!(state.cycle().map(|m| m.name.as_str()), Some("Volume"));
    }

    #[test]
    fn cycling_skips_a_mode_that_assigns_nothing() {
        let mut state = CrownModeState::new(vec![
            mode("Volume", Action::VolumeUp),
            inert("Empty"),
            mode("Scroll", Action::ScrollUp),
        ]);
        assert_eq!(state.cycle().map(|m| m.name.as_str()), Some("Scroll"));
    }

    /// All-inert must not spin forever or silently move the index.
    #[test]
    fn an_all_inert_list_never_cycles() {
        let mut state = CrownModeState::new(vec![inert("A"), inert("B")]);
        assert!(state.cycle().is_none());
        assert_eq!(state.index, 0, "the index must not drift");
    }

    #[test]
    fn an_empty_list_never_cycles() {
        let mut state = CrownModeState::new(Vec::new());
        assert!(state.cycle().is_none());
        assert!(state.active().is_none());
    }

    #[test]
    fn only_the_controls_the_mode_sets_resolve() {
        let state = CrownModeState::new(vec![mode("Volume", Action::VolumeUp)]);
        assert_eq!(
            state.action_for(ButtonId::CrownRotateUp),
            Some(&Action::VolumeUp)
        );
        assert_eq!(state.action_for(ButtonId::CrownRotateDown), None);
        assert_eq!(state.action_for(ButtonId::CrownTap), None);
    }

    #[test]
    fn the_indicator_lists_every_mode_and_marks_the_active_one() {
        let mut state = CrownModeState::new(vec![
            mode("Volume", Action::VolumeUp),
            mode("Scroll", Action::ScrollUp),
        ]);
        state.cycle();
        let hud = state
            .indicator()
            .expect("a non-empty list has an indicator");
        assert_eq!(hud.names, vec!["Volume".to_string(), "Scroll".to_string()]);
        assert_eq!(hud.selected, 1);
    }

    #[test]
    fn an_empty_list_has_no_indicator() {
        assert!(CrownModeState::new(Vec::new()).indicator().is_none());
    }

    #[test]
    fn a_device_without_modes_resolves_to_nothing() {
        let modes = CrownModes::default();
        assert_eq!(
            modes.action_for(Some("2b034"), ButtonId::CrownRotateUp),
            None
        );
        assert_eq!(modes.action_for(None, ButtonId::CrownRotateUp), None);
    }

    /// An unrelated config edit must not yank the dial back to mode one.
    #[test]
    fn republishing_an_unchanged_list_keeps_the_position() {
        let list = vec![
            mode("Volume", Action::VolumeUp),
            mode("Scroll", Action::ScrollUp),
        ];
        let mut modes = CrownModes::default();
        modes.republish(HashMap::from([("k".to_string(), list.clone())]));
        modes.state_for(Some("k")).expect("state").cycle();
        assert_eq!(modes.active_name(Some("k")), Some("Scroll"));

        modes.republish(HashMap::from([("k".to_string(), list)]));
        assert_eq!(
            modes.active_name(Some("k")),
            Some("Scroll"),
            "an unchanged list must not reset the active mode"
        );
    }

    /// A changed list invalidates the old position.
    #[test]
    fn republishing_a_changed_list_resets_the_position() {
        let mut modes = CrownModes::default();
        modes.republish(HashMap::from([(
            "k".to_string(),
            vec![
                mode("Volume", Action::VolumeUp),
                mode("Scroll", Action::ScrollUp),
            ],
        )]));
        modes.state_for(Some("k")).expect("state").cycle();
        assert_eq!(modes.active_name(Some("k")), Some("Scroll"));

        modes.republish(HashMap::from([(
            "k".to_string(),
            vec![
                mode("Zoom", Action::ScrollUp),
                mode("Tabs", Action::NextTab),
            ],
        )]));
        assert_eq!(modes.active_name(Some("k")), Some("Zoom"));
    }

    #[test]
    fn republishing_drops_a_device_that_lost_its_modes() {
        let mut modes = CrownModes::default();
        modes.republish(HashMap::from([(
            "k".to_string(),
            vec![mode("Volume", Action::VolumeUp)],
        )]));
        assert!(modes.active_name(Some("k")).is_some());
        modes.republish(HashMap::new());
        assert!(modes.active_name(Some("k")).is_none());
    }
}
