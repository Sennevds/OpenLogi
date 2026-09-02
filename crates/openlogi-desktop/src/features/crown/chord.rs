//! Press-and-rotate: the crown press as a chord holder.
//!
//! Holding the dial and turning it runs a direction; releasing without turning
//! runs the click slot. That is the raw-XY gesture button's shape, so this
//! reuses its config machinery — `Binding::Gesture` on `ButtonId::CrownPress`,
//! toggled through [`AppState::commit_gesture_mode`] and edited through
//! [`AppState::commit_gesture_binding`] — rather than inventing a parallel one.
//!
//! Gesture mode lives in the device's global bindings (a per-app entry holds
//! one action and has no per-direction shape), so the entry point is hidden
//! inside a per-app profile and the state layer refuses it there as a backstop.

use std::collections::BTreeMap;

use gpui::{
    Entity, ParentElement, Role, StatefulInteractiveElement as _, Styled, div,
    prelude::FluentBuilder as _, svg,
};
use gpui_component::{IconName, Selectable as _, h_flex, v_flex};
use openlogi_core::binding::{Action, ButtonId, GestureDirection};

use super::{CrownPanel, Target};
use crate::features::mouse::picker::{action_icon_path, compact_panel};
use crate::state::AppState;
use crate::ui::action::localized_action_label;
use crate::ui::components::{MenuRow, control_button};
use crate::ui::section::section_label;
use crate::ui::theme::{Palette, Typography as _};

/// The chord's three slots, in the order the card lists them. `Left`/`Right`
/// are absent on purpose: a dial has one axis, and offering slots no rotation
/// can ever reach would be dead configuration.
pub(super) const CHORD_SLOTS: [GestureDirection; 3] = [
    GestureDirection::Up,
    GestureDirection::Down,
    GestureDirection::Click,
];

/// The slot's name, for a picker caption.
pub(super) fn slot_caption(direction: GestureDirection) -> String {
    slot_label(direction).to_string()
}

/// What each slot means for a dial, which is not what it means for a mouse —
/// hence its own labels rather than [`GestureDirection`]'s swipe vocabulary.
fn slot_label(direction: GestureDirection) -> gpui::SharedString {
    match direction {
        GestureDirection::Up => tr!("Turn up while held"),
        GestureDirection::Down => tr!("Turn down while held"),
        GestureDirection::Click => tr!("Press without turning"),
        GestureDirection::Left | GestureDirection::Right => tr!("Off"),
    }
}

/// The press-and-rotate card: its three slots and the way back out.
pub(super) fn chord_card(
    map: &BTreeMap<GestureDirection, Action>,
    selected: Option<Target>,
    panel: &Entity<CrownPanel>,
    pal: Palette,
) -> gpui::Div {
    v_flex()
        .gap_2()
        .w_full()
        .child(
            v_flex()
                .w_full()
                .gap_0p5()
                .child(section_label(tr!("Press and rotate"), pal).w_full())
                .child(
                    div()
                        .text_caption()
                        .text_color(pal.text_muted)
                        .child(tr!("Turning the dial while it is held runs these instead.")),
                ),
        )
        .child(
            compact_panel(pal)
                .w_full()
                .children(CHORD_SLOTS.iter().copied().map(|direction| {
                    slot_row(
                        direction,
                        map.get(&direction),
                        selected == Some(Target::Chord(direction)),
                        panel,
                        pal,
                    )
                })),
        )
        .child(
            control_button("crown-chord-off")
                .w_full()
                .icon(IconName::Undo)
                .label(tr!("Stop using press and rotate"))
                .on_click(|_event, _window, cx| {
                    AppState::update_bindings(cx, |state| {
                        state.commit_gesture_mode(ButtonId::CrownPress, false);
                    });
                }),
        )
}

/// One chord slot: what it is, and the action it runs.
fn slot_row(
    direction: GestureDirection,
    action: Option<&Action>,
    selected: bool,
    panel: &Entity<CrownPanel>,
    pal: Palette,
) -> MenuRow {
    let panel = panel.clone();
    // An unset slot and one deliberately set to `None` both do nothing, so
    // both read as "Off" — the distinction the mode editor draws does not
    // exist here, where there is no ordinary binding to fall through to.
    let bound = action.is_some_and(|action| *action != Action::None);
    let label = if bound {
        action.map_or_else(|| tr!("Off"), localized_action_label)
    } else {
        tr!("Off")
    };
    MenuRow::new(("crown-chord-slot", direction as usize))
        .selected(selected)
        .role(Role::MenuItem)
        .aria_label(slot_label(direction))
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .child(
                    svg()
                        .path(action_icon_path(action.unwrap_or(&Action::None)))
                        .size_4()
                        .flex_none()
                        .text_color(pal.text_muted),
                )
                .child(div().child(slot_label(direction))),
        )
        .child(
            div()
                .text_caption()
                .text_color(if bound {
                    pal.text_primary
                } else {
                    pal.text_muted
                })
                .child(label),
        )
        .on_click(move |_event, _window, cx| {
            panel.update(cx, |panel, vcx| {
                panel.select(Target::Chord(direction), vcx);
            });
        })
}

/// The way in, offered at the foot of the crown press's action picker.
///
/// Hidden inside a per-app profile: gesture mode is a property of the device's
/// global bindings, so offering it there would silently change every
/// application instead of the one on screen.
pub(super) fn enable_row(in_app_profile: bool, pal: Palette) -> gpui::Div {
    v_flex()
        .w_full()
        .when(in_app_profile, |row| {
            row.child(
                div()
                    .px_2()
                    .pb_1()
                    .text_caption()
                    .text_color(pal.text_muted)
                    .child(tr!(
                        "Press and rotate is set per device, not per application."
                    )),
            )
        })
        .when(!in_app_profile, |row| {
            row.child(
                control_button("crown-chord-on")
                    .w_full()
                    .icon(IconName::Plus)
                    .label(tr!("Use press and rotate"))
                    .on_click(|_event, _window, cx| {
                        AppState::update_bindings(cx, |state| {
                            state.commit_gesture_mode(ButtonId::CrownPress, true);
                        });
                    }),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dial has one rotation axis, so the card must never offer a slot no
    /// turn can reach — a `Left`/`Right` row would be dead configuration.
    #[test]
    fn the_card_offers_only_slots_a_dial_can_reach() {
        assert!(!CHORD_SLOTS.contains(&GestureDirection::Left));
        assert!(!CHORD_SLOTS.contains(&GestureDirection::Right));
        assert!(CHORD_SLOTS.contains(&GestureDirection::Click));
    }

    /// Every offered slot needs a dial-specific label; the swipe vocabulary
    /// `GestureDirection` carries would read as nonsense on a crown.
    #[test]
    fn every_offered_slot_has_its_own_label() {
        for direction in CHORD_SLOTS {
            assert_ne!(
                slot_label(direction),
                tr!("Off"),
                "{direction:?} falls through to the placeholder label"
            );
        }
    }
}
