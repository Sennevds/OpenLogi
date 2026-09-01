//! Crown-mode list editing for the selected device.
//!
//! The mode list is what a crown tap cycles through, and it is per-device
//! *and* per-application: every edit here lands in whichever profile
//! [`AppState::editing_app`] has open, exactly like a button binding. The
//! difference is that a per-app override replaces the whole list rather than
//! one entry (see `DeviceConfig::per_app_crown_modes`), so the first edit made
//! inside an app profile forks the default list into that app — which is what
//! the panel is showing at the time, so nothing appears to move.

use openlogi_core::binding::{Action, ButtonId, CrownMode};

use super::{AppState, DeviceRecord};

impl AppState {
    /// The crown-mode list the open profile edits, in cycle order.
    ///
    /// Inside an app profile with no list of its own this is the device's
    /// default list — inherited, not owned — so the panel shows what that app
    /// actually cycles today. [`Self::crown_modes_are_overridden`] tells the
    /// two apart.
    #[must_use]
    pub fn current_crown_modes(&self) -> Vec<CrownMode> {
        let Some(key) = self
            .current_record()
            .and_then(DeviceRecord::persistent_config_key)
        else {
            return Vec::new();
        };
        match self.editing_app() {
            Some(app) => self
                .config
                .per_app_crown_modes(key, app)
                .cloned()
                .unwrap_or_else(|| self.config.crown_modes(key)),
            None => self.config.crown_modes(key),
        }
    }

    /// Whether the open per-app profile carries its own mode list rather than
    /// inheriting the device's. Always `false` in the global profile, which has
    /// nothing to inherit from.
    #[must_use]
    pub fn crown_modes_are_overridden(&self) -> bool {
        let Some(key) = self
            .current_record()
            .and_then(DeviceRecord::persistent_config_key)
        else {
            return false;
        };
        self.editing_app()
            .is_some_and(|app| self.config.per_app_crown_modes(key, app).is_some())
    }

    /// Append a mode named `name` to the open profile's list.
    pub fn add_crown_mode(&mut self, name: String) {
        let mut modes = self.current_crown_modes();
        modes.push(CrownMode {
            name,
            ..CrownMode::default()
        });
        self.commit_crown_modes(modes, "crown mode");
    }

    /// Drop the mode at `index`.
    ///
    /// Removing the last one inside an app profile drops the override, so that
    /// app inherits the device's list again — an empty override would be
    /// indistinguishable from "inherit" in the config, and inheriting is the
    /// reading that loses no user intent.
    pub fn remove_crown_mode(&mut self, index: usize) {
        let mut modes = self.current_crown_modes();
        if index >= modes.len() {
            return;
        }
        modes.remove(index);
        self.commit_crown_modes(modes, "crown mode");
    }

    /// Rename the mode at `index`. A blank name is refused: the name is the
    /// only thing the on-screen indicator can show for a mode.
    pub fn rename_crown_mode(&mut self, index: usize, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let mut modes = self.current_crown_modes();
        let Some(mode) = modes.get_mut(index) else {
            return;
        };
        if mode.name == name {
            return;
        }
        mode.name = name.to_string();
        self.commit_crown_modes(modes, "crown mode name");
    }

    /// Set one control inside the mode at `index`, or clear it with `None` so
    /// that control falls back to its ordinary binding while the mode is
    /// active.
    ///
    /// A control a mode cannot own — the tap, which is what cycles modes — is
    /// ignored rather than stored, mirroring `CrownMode::action_for`.
    pub fn commit_crown_mode_action(
        &mut self,
        index: usize,
        control: ButtonId,
        action: Option<Action>,
    ) {
        let mut modes = self.current_crown_modes();
        let Some(mode) = modes.get_mut(index) else {
            return;
        };
        let slot = match control {
            ButtonId::CrownRotateUp => &mut mode.rotate_up,
            ButtonId::CrownRotateDown => &mut mode.rotate_down,
            ButtonId::CrownPress => &mut mode.press,
            _ => return,
        };
        if *slot == action {
            return;
        }
        *slot = action;
        self.commit_crown_modes(modes, "crown mode action");
    }

    /// Drop the open app profile's mode list, so the app cycles the device's
    /// default one again. A no-op in the global profile.
    pub fn clear_app_crown_modes(&mut self) {
        if self.editing_app().is_none() {
            return;
        }
        self.commit_crown_modes(Vec::new(), "crown mode override");
    }

    /// Write `modes` into whichever profile is open and reload the agent.
    fn commit_crown_modes(&mut self, modes: Vec<CrownMode>, what: &'static str) {
        let Some(key) = self
            .current_record()
            .and_then(DeviceRecord::persistent_config_key)
            .map(str::to_string)
        else {
            return;
        };
        let app = self.editing_app().map(str::to_string);
        self.config.edit(|config| match app {
            Some(app) => config.set_per_app_crown_modes(&key, &app, modes),
            None => config.set_crown_modes(&key, modes),
        });
        self.persist_and_reload(what);
    }
}
