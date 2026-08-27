//! The crown-dial binding panel — the Crown tab body.
//!
//! The Craft keyboard's crown is a rotary dial with four bindable controls
//! ([`ButtonId::CROWN_CONTROLS`]): the two rotation directions, the press, and
//! a touch tap. Selecting one on the left opens the shared action catalog on
//! the right, mirroring the function-row remapper's select-then-pick model.
//!
//! Deliberately a list rather than a hardware diagram: the crown has no marker
//! in Logi's keyboard metadata, and hotspot positions are never synthesized
//! (see `.claude/rules/gui.md`).
//!
//! Crown bindings are per-device (`config.devices[key].bindings`), so this
//! commits through [`AppState::commit_binding`] like the mouse buttons — and
//! therefore honours the per-app profile the user has open.
//!
//! An unbound control keeps the dial's firmware behaviour (volume): the agent
//! only diverts the crown while one of the four carries a real action, so the
//! panel's "Off" is a live state, not just an empty slot.

use std::rc::Rc;

// `StatefulInteractiveElement` is imported unconditionally on purpose: a
// cfg-gated trait import compiles locally and breaks the other CI lanes the
// moment an ungated element calls one of its methods (see gui.md).
use gpui::{
    Context, Entity, IntoElement, ParentElement, Render, Role, StatefulInteractiveElement as _,
    Styled, Subscription, Window, div, prelude::FluentBuilder as _, px, svg,
};
use gpui_component::{IconName, Selectable as _, h_flex, v_flex};
use openlogi_core::binding::{Action, ButtonId, default_binding};

use crate::features::mouse::picker::{
    PickFn, action_icon_path, action_rows, compact_panel, divider, editor_scroll_list,
};
use crate::state::{AppState, StateEvent};
use crate::ui::action::localized_action_label;
use crate::ui::components::{MenuRow, control_button};
use crate::ui::section::section_label;
use crate::ui::theme::{self, Palette, Typography as _};

/// Width of the action-picker card, matching the function row's panel.
const PANEL_W: f32 = 320.;
/// Width of the control list beside it.
const LIST_W: f32 = 260.;

/// One crown control resolved for display.
struct CrownSlot {
    button: ButtonId,
    /// The action in force, already resolved through the device's defaults.
    action: Action,
}

/// The crown-dial binding panel.
pub struct CrownPanel {
    /// Index into [`ButtonId::CROWN_CONTROLS`], or `None` when the picker is
    /// closed.
    selected: Option<usize>,
    _state_obs: Subscription,
}

impl CrownPanel {
    /// Create the panel.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let state_obs = cx.subscribe(&AppState::global(cx), |_view, _, event: &StateEvent, cx| {
            let relevant = match event {
                StateEvent::InventoryChanged | StateEvent::DeviceSelected(_) => true,
                StateEvent::BindingsChanged(key) => AppState::try_read(cx)
                    .and_then(AppState::current_record)
                    .is_some_and(|record| record.device_key() == *key),
                _ => false,
            };
            if relevant {
                cx.notify();
            }
        });
        Self {
            selected: None,
            _state_obs: state_obs,
        }
    }

    /// Toggle the picker for one control: clicking the open one closes it.
    fn click_control(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.selected = if self.selected == Some(idx) {
            None
        } else {
            Some(idx)
        };
        cx.notify();
    }

    /// The action-catalog card for the selected control.
    fn picker_panel(slot: &CrownSlot, view: &Entity<Self>, cx: &mut Context<Self>) -> gpui::Div {
        let pal = theme::palette(cx);
        let button = slot.button;
        let current = catalog_selection(&slot.action).cloned();

        let view_for_pick = view.clone();
        let on_pick: PickFn = Rc::new(move |action, _window, cx| {
            AppState::update_bindings(cx, |state| state.commit_binding(button, action));
            view_for_pick.update(cx, |_, vcx| vcx.notify());
        });

        let rows = action_rows("crown-action", current.as_ref(), &on_pick, pal);

        // Only while a per-app profile is open, and only for a control that
        // actually carries an override — otherwise there is nothing to revert
        // to and the button would be a no-op. Picking the catalog's own "None"
        // is not the same thing: that stores "do nothing in this app", where
        // this drops the override so the default profile applies again.
        let overridden = AppState::try_read(cx)
            .and_then(AppState::editing_app_overrides)
            .is_some_and(|overrides| overrides.contains_key(&button));
        let view_for_clear = view.clone();

        compact_panel(pal)
            .w(px(PANEL_W))
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .text_caption()
                    .text_color(pal.text_muted)
                    .child(tr!("Bind %{name}", name => tr!(button.label()).to_string())),
            )
            .child(divider(pal))
            .child(editor_scroll_list("crown-panel-scroll", rows))
            .when(overridden, |panel| {
                panel.child(divider(pal)).child(
                    control_button("crown-use-default")
                        .w_full()
                        .icon(IconName::Undo)
                        .label(tr!("Use the default profile"))
                        .on_click(move |_event, _window, cx| {
                            AppState::update_bindings(cx, |state| {
                                state.clear_app_binding(button);
                            });
                            view_for_clear.update(cx, |_, vcx| vcx.notify());
                        }),
                )
            })
    }
}

impl Render for CrownPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = theme::palette(cx);
        let bindings = AppState::try_read(cx).map(AppState::button_bindings);
        let slots: Vec<CrownSlot> = ButtonId::CROWN_CONTROLS
            .iter()
            .copied()
            .map(|button| {
                // Crown controls sit outside `ButtonId::ALL`, so the projection
                // omits an unbound one entirely — fall back to its default,
                // which is `Action::None` for every crown control.
                let action = bindings
                    .as_ref()
                    .and_then(|bindings| bindings.get(&button).cloned())
                    .unwrap_or_else(|| default_binding(button));
                CrownSlot { button, action }
            })
            .collect();

        let view = cx.entity();
        let picker = self
            .selected
            .and_then(|idx| slots.get(idx))
            .map(|slot| Self::picker_panel(slot, &view, cx));

        v_flex()
            .gap_3()
            .w_full()
            .child(
                section_label(
                    tr!("Unbound controls keep the dial's volume function."),
                    pal,
                )
                .w_full(),
            )
            .child(
                h_flex()
                    .gap_4()
                    .items_start()
                    .child(control_list(&slots, self.selected, &view, pal))
                    .children(picker),
            )
    }
}

/// The four crown controls, each showing the action in force.
fn control_list(
    slots: &[CrownSlot],
    selected: Option<usize>,
    view: &Entity<CrownPanel>,
    pal: Palette,
) -> gpui::Div {
    compact_panel(pal).w(px(LIST_W)).children(
        slots
            .iter()
            .enumerate()
            .map(|(idx, slot)| control_row(idx, slot, selected == Some(idx), view, pal)),
    )
}

/// Which catalog row to tick for a control's current action.
///
/// An inert control resolves to [`Action::None`] from its default, not from a
/// stored choice, so nothing is ticked — otherwise the catalog's own "None"
/// row would read back as a decision the user had made, and the panel could
/// not distinguish "never bound" from "deliberately disabled".
fn catalog_selection(action: &Action) -> Option<&Action> {
    (*action != Action::None).then_some(action)
}

/// One selectable control row: its name, and the action it runs.
fn control_row(
    idx: usize,
    slot: &CrownSlot,
    selected: bool,
    view: &Entity<CrownPanel>,
    pal: Palette,
) -> MenuRow {
    let view = view.clone();
    let bound = slot.action != Action::None;
    let action_label = if bound {
        localized_action_label(&slot.action)
    } else {
        tr!("Off")
    };
    // Keyed by the control itself, not the row position, so the list can be
    // reordered without moving focus between controls.
    MenuRow::new(("crown-control", idx))
        .selected(selected)
        .role(Role::MenuItem)
        .aria_label(tr!(slot.button.label()))
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .child(
                    svg()
                        .path(action_icon_path(&slot.action))
                        .size_4()
                        .flex_none()
                        .text_color(pal.text_muted),
                )
                .child(div().child(tr!(slot.button.label()))),
        )
        .child(
            div()
                .text_caption()
                .text_color(if bound {
                    pal.text_primary
                } else {
                    pal.text_muted
                })
                .child(action_label),
        )
        .on_click(move |_event, _window, cx| {
            view.update(cx, |panel, vcx| panel.click_control(idx, vcx));
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inert_control_ticks_no_catalog_row() {
        assert_eq!(catalog_selection(&Action::None), None);
    }

    #[test]
    fn a_bound_control_ticks_its_own_action() {
        assert_eq!(
            catalog_selection(&Action::VolumeUp),
            Some(&Action::VolumeUp)
        );
    }

    /// The panel renders one row per crown control, so a control added to
    /// [`ButtonId::CROWN_CONTROLS`] without a label would show up blank.
    #[test]
    fn every_crown_control_has_a_label() {
        for button in ButtonId::CROWN_CONTROLS {
            assert!(
                !button.label().is_empty(),
                "{button:?} would render an unnamed row"
            );
        }
    }
}
