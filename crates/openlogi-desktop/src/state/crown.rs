//! Crown-mode list editing for the selected device.
//!
//! The mode list is what a crown tap cycles through, and it is per-device *and*
//! per-application: every edit here lands in whichever profile
//! [`AppState::editing_app`] has open, like a button binding.
//!
//! Unlike a binding, a per-app override replaces the whole *list* rather than
//! one entry (see `DeviceConfig::per_app_crown_modes`) — the list is a cycle
//! order, and merging entries into it by position would change what the next
//! tap reaches. That makes ownership a state the editor has to show rather than
//! infer, so an app is in exactly one of two states:
//!
//! - **inherited** — no list of its own; it cycles the device's. Nothing here
//!   edits it, because editing would silently fork the device's list into this
//!   app. [`AppState::start_app_crown_modes`] and
//!   [`AppState::copy_default_crown_modes`] are the two explicit ways out.
//! - **owned** — its own list, which may legitimately be *empty* (a dial that
//!   cycles nothing). Deleting the last mode leaves it owned and empty; only
//!   [`AppState::clear_app_crown_modes`] hands the app back to the device's
//!   list.

use openlogi_core::binding::{Action, ButtonId, CrownMode};

use super::{AppState, DeviceRecord};

/// Whether the list the panel is showing belongs to the open profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrownModeOwnership {
    /// The open profile owns this list and may edit it.
    Owned,
    /// A per-app profile with no list of its own, showing the device's.
    Inherited,
}

impl AppState {
    /// The crown-mode list the open profile shows, in cycle order.
    ///
    /// Pair it with [`Self::crown_mode_ownership`]: an inherited list is the
    /// device's, shown for reference, and must not be edited in place.
    #[must_use]
    pub fn current_crown_modes(&self) -> Vec<CrownMode> {
        let Some(key) = self.crown_config_key() else {
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

    /// Whether the open profile owns the list [`Self::current_crown_modes`]
    /// returned. The device's own profile always owns its list.
    #[must_use]
    pub fn crown_mode_ownership(&self) -> CrownModeOwnership {
        let owned = match (self.crown_config_key(), self.editing_app()) {
            (Some(key), Some(app)) => self.config.per_app_crown_modes(key, app).is_some(),
            _ => true,
        };
        if owned {
            CrownModeOwnership::Owned
        } else {
            CrownModeOwnership::Inherited
        }
    }

    /// Give the open app profile its own — empty — mode list.
    ///
    /// Empty, not a copy of the device's: an app profile that silently copied
    /// the default list would leave the user editing a duplicate they never
    /// asked for. [`Self::copy_default_crown_modes`] is the copy, when that is
    /// what they want.
    pub fn start_app_crown_modes(&mut self) {
        if self.editing_app().is_none() {
            return;
        }
        self.commit_crown_modes(Vec::new(), "crown modes");
    }

    /// Fork the device's list into the open app profile, so it can be edited
    /// from there without affecting other applications.
    pub fn copy_default_crown_modes(&mut self) {
        let Some(key) = self.crown_config_key() else {
            return;
        };
        if self.editing_app().is_none() {
            return;
        }
        let modes = self.config.crown_modes(key);
        self.commit_crown_modes(modes, "crown modes");
    }

    /// Drop the open app profile's list, so the app cycles the device's again.
    /// A no-op in the device's own profile, which has nothing to inherit.
    pub fn clear_app_crown_modes(&mut self) {
        let Some(key) = self.crown_config_key().map(str::to_string) else {
            return;
        };
        let Some(app) = self.editing_app().map(str::to_string) else {
            return;
        };
        self.config
            .edit(|config| config.clear_per_app_crown_modes(&key, &app));
        self.persist_and_reload("crown mode override");
    }

    /// Append a mode named `name` to the open profile's list.
    ///
    /// Refused on an inherited list: the caller must claim ownership first, so
    /// adding a mode inside an app profile can never quietly fork the device's
    /// list.
    pub fn add_crown_mode(&mut self, name: String) {
        if self.crown_mode_ownership() == CrownModeOwnership::Inherited {
            return;
        }
        let mut modes = self.current_crown_modes();
        modes.push(CrownMode {
            name,
            ..CrownMode::default()
        });
        self.commit_crown_modes(modes, "crown mode");
    }

    /// A default name for the next mode, distinct from every existing one so
    /// two unnamed modes are never indistinguishable in the mode indicator.
    #[must_use]
    pub fn next_crown_mode_name(&self, template: impl Fn(usize) -> String) -> String {
        let existing = self.current_crown_modes();
        (1..=existing.len() + 1)
            .map(&template)
            .find(|candidate| existing.iter().all(|mode| &mode.name != candidate))
            .unwrap_or_else(|| template(existing.len() + 1))
    }

    /// Drop the mode at `index`.
    ///
    /// An app profile keeps its list when the last mode goes: it then owns an
    /// empty one, and its dial cycles nothing. Inheriting the device's list
    /// again is [`Self::clear_app_crown_modes`], never a side effect of a
    /// delete — that is what made deleting the last mode appear to undo the
    /// previous delete.
    pub fn remove_crown_mode(&mut self, index: usize) {
        if self.crown_mode_ownership() == CrownModeOwnership::Inherited {
            return;
        }
        let mut modes = self.current_crown_modes();
        if index >= modes.len() {
            return;
        }
        modes.remove(index);
        self.commit_crown_modes(modes, "crown mode");
    }

    /// Rename the mode at `index`. A blank name is refused: the name is the
    /// only thing the mode indicator can show.
    pub fn rename_crown_mode(&mut self, index: usize, name: &str) {
        if self.crown_mode_ownership() == CrownModeOwnership::Inherited {
            return;
        }
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
        if self.crown_mode_ownership() == CrownModeOwnership::Inherited {
            return;
        }
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

    /// The active device's config key, when it can carry saved settings.
    fn crown_config_key(&self) -> Option<&str> {
        self.current_record()
            .and_then(DeviceRecord::persistent_config_key)
    }

    /// Write `modes` into whichever profile is open and reload the agent.
    fn commit_crown_modes(&mut self, modes: Vec<CrownMode>, what: &'static str) {
        let Some(key) = self.crown_config_key().map(str::to_string) else {
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
