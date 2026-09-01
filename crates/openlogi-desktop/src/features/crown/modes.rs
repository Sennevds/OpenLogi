//! The crown-mode list: what a tap cycles the dial through.
//!
//! One card per mode, each carrying the three controls a mode may own. The tap
//! is deliberately absent from that set — it is what cycles modes, so a mode
//! that could rebind it would strand the user inside itself.
//!
//! A per-app list replaces the device's wholesale rather than merging into it,
//! so **ownership is shown, never inferred**: an app profile with no list of
//! its own renders the device's list read-only, and two explicit buttons are
//! the only ways to start editing. Letting an edit fork the device's list
//! silently is what made an app profile look like it had copied the default.
//!
//! Rendering only — every edit goes through `AppState`'s crown-mode commits.

use gpui::{
    Entity, IntoElement, ParentElement, Role, StatefulInteractiveElement as _, Styled, div,
    prelude::FluentBuilder as _, px, svg,
};
use gpui_component::{
    Icon, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use openlogi_core::binding::{Action, ButtonId, CrownMode};

use super::{CrownPanel, LIST_W, Target};
use crate::features::mouse::picker::{action_icon_path, compact_panel, divider};
use crate::state::{AppState, CrownModeOwnership};
use crate::ui::action::localized_action_label;
use crate::ui::components::{MenuRow, control_button};
use crate::ui::section::section_label;
use crate::ui::theme::{Palette, Typography as _};

/// The controls a mode may own, in the order the cards list them.
pub(super) const MODE_CONTROLS: [ButtonId; 3] = [
    ButtonId::CrownRotateUp,
    ButtonId::CrownRotateDown,
    ButtonId::CrownPress,
];

/// Everything the section needs about the list it is drawing.
pub(super) struct ModesView<'a> {
    /// The modes in cycle order.
    pub modes: &'a [CrownMode],
    /// Whether the open profile owns them or is only showing the device's.
    pub ownership: CrownModeOwnership,
    /// Whether a per-app profile is open, which decides the revert affordance.
    pub in_app_profile: bool,
    /// Whether any crown control actually runs `CycleCrownMode`.
    pub cycles_modes: bool,
    /// What the picker beside the list is open on.
    pub selected: Option<Target>,
}

impl ModesView<'_> {
    fn editable(&self) -> bool {
        self.ownership == CrownModeOwnership::Owned
    }
}

/// The whole modes section: heading, one card per mode, and the ownership or
/// add affordances the current state allows.
pub(super) fn modes_section(
    view: &ModesView<'_>,
    panel: &Entity<CrownPanel>,
    pal: Palette,
) -> gpui::Div {
    let editable = view.editable();
    v_flex()
        .gap_2()
        .w(px(LIST_W))
        .child(heading(view, pal))
        .children(
            view.modes
                .iter()
                .enumerate()
                .map(|(index, mode)| mode_card(index, mode, view, panel, pal)),
        )
        .when(view.modes.is_empty() && editable, |section| {
            section.child(
                compact_panel(pal).child(
                    div()
                        .p_2()
                        .text_caption()
                        .text_color(pal.text_muted)
                        .child(tr!("No modes yet — the dial uses the bindings above.")),
                ),
            )
        })
        .when(editable, |section| section.child(add_mode_button()))
        .when(!editable, |section| {
            // The two explicit ways to start editing. Nothing else in this
            // panel writes a per-app list, so an app profile can never end up
            // holding a copy of the device's modes the user did not ask for.
            section
                .child(
                    control_button("crown-modes-own")
                        .w_full()
                        .icon(IconName::Plus)
                        .label(tr!("Give this app its own modes"))
                        .on_click(|_event, _window, cx| {
                            AppState::update_bindings(cx, AppState::start_app_crown_modes);
                        }),
                )
                .child(
                    control_button("crown-modes-copy")
                        .w_full()
                        .icon(IconName::Copy)
                        .label(tr!("Copy the default modes"))
                        .on_click(|_event, _window, cx| {
                            AppState::update_bindings(cx, AppState::copy_default_crown_modes);
                        }),
                )
        })
        .when(editable && view.in_app_profile, |section| {
            section.child(
                control_button("crown-modes-use-default")
                    .w_full()
                    .icon(IconName::Undo)
                    .label(tr!("Use the default profile's modes"))
                    .on_click(|_event, _window, cx| {
                        AppState::update_bindings(cx, AppState::clear_app_crown_modes);
                    }),
            )
        })
}

/// Section heading plus the one line of state worth saying out loud: whose
/// modes these are, or that nothing can reach them.
fn heading(view: &ModesView<'_>, pal: Palette) -> gpui::Div {
    let unreachable = !view.modes.is_empty() && !view.cycles_modes;
    let inherited = !view.editable();
    let caption = if inherited {
        tr!("This app cycles the default profile's modes.")
    } else if unreachable {
        tr!("Bind a control to Cycle Crown Mode to reach these.")
    } else {
        tr!("Cycled by whichever control runs Cycle Crown Mode.")
    };
    v_flex()
        .w_full()
        .gap_0p5()
        .child(section_label(tr!("Modes"), pal).w_full())
        .child(
            div()
                .text_caption()
                .text_color(if unreachable && !inherited {
                    pal.text_primary
                } else {
                    pal.text_muted
                })
                .child(caption),
        )
}

/// One mode: its name, the rename and remove buttons, and its three controls.
///
/// An inherited card is inert — no buttons, no selectable rows — because every
/// edit it could offer would fork the device's list into this app.
fn mode_card(
    index: usize,
    mode: &CrownMode,
    view: &ModesView<'_>,
    panel: &Entity<CrownPanel>,
    pal: Palette,
) -> gpui::Div {
    compact_panel(pal)
        .w_full()
        .child(card_header(index, mode, view.editable(), panel, pal))
        .child(divider(pal))
        .children(MODE_CONTROLS.iter().copied().map(|control| {
            mode_control_row(
                index,
                control,
                mode.action_for(control),
                MoreRow {
                    selected: view.selected == Some(Target::ModeControl(index, control)),
                    editable: view.editable(),
                },
                panel,
                pal,
            )
        }))
}

fn card_header(
    index: usize,
    mode: &CrownMode,
    editable: bool,
    panel: &Entity<CrownPanel>,
    pal: Palette,
) -> gpui::Div {
    let panel_for_rename = panel.clone();
    h_flex()
        .w_full()
        .px_2()
        .py_1()
        .gap_1()
        .items_center()
        .child(div().flex_1().min_w_0().text_body().child(mode.name.clone()))
        // A mode that assigns nothing is skipped by the cycle rather than
        // stopped on, so it must not look like a working position.
        .children(mode.is_inert().then(|| {
            div()
                .text_caption()
                .text_color(pal.text_muted)
                .child(tr!("Empty"))
        }))
        .when(editable, |header| {
            header
                .child(
                    Button::new(("crown-mode-rename", index))
                        .xsmall()
                        .ghost()
                        .icon(Icon::empty().path("action-icons/pencil.svg"))
                        .on_click(move |_event, _window, cx| {
                            panel_for_rename.update(cx, |panel, vcx| {
                                panel.select(Target::Mode(index), vcx);
                            });
                        }),
                )
                .child(
                    Button::new(("crown-mode-remove", index))
                        .xsmall()
                        .ghost()
                        .icon(IconName::Close)
                        .on_click(move |_event, _window, cx| {
                            AppState::update_bindings(cx, |state| state.remove_crown_mode(index));
                        }),
                )
        })
}

/// The two per-row flags, kept together so the row helper stays inside the
/// argument-count limit.
#[derive(Clone, Copy)]
struct MoreRow {
    selected: bool,
    editable: bool,
}

/// One control inside a mode. "Falls through" is the honest label for an unset
/// one: the mode leaves that control on its ordinary binding rather than
/// disabling it.
fn mode_control_row(
    index: usize,
    control: ButtonId,
    action: Option<&Action>,
    row: MoreRow,
    panel: &Entity<CrownPanel>,
    pal: Palette,
) -> MenuRow {
    let panel = panel.clone();
    let label = action.map_or_else(|| tr!("Falls through"), localized_action_label);
    // Keyed by mode and control rather than row position, so adding a mode does
    // not move focus between existing rows.
    MenuRow::new((
        "crown-mode-control",
        index * MODE_CONTROLS.len() + control_slot(control),
    ))
    .selected(row.selected)
    .role(Role::MenuItem)
    .aria_label(tr!(control.label()))
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
            .child(div().child(tr!(control.label()))),
    )
    .child(
        div()
            .text_caption()
            .text_color(if action.is_some() {
                pal.text_primary
            } else {
                pal.text_muted
            })
            .child(label),
    )
    .on_click(move |_event, _window, cx| {
        if !row.editable {
            return;
        }
        panel.update(cx, |panel, vcx| {
            panel.select(Target::ModeControl(index, control), vcx);
        });
    })
}

/// Stable per-control offset, so a row's element id follows the control rather
/// than its position in [`MODE_CONTROLS`].
fn control_slot(control: ButtonId) -> usize {
    MODE_CONTROLS
        .iter()
        .position(|candidate| *candidate == control)
        .unwrap_or(MODE_CONTROLS.len())
}

/// Adds a mode immediately, the way the DPI panel's "+" adds a preset —
/// one click, named `Mode N`, renamed afterwards if the default won't do. A
/// dialog in front of this would be one more thing between the user and a list
/// they are still assembling.
fn add_mode_button() -> impl IntoElement {
    control_button("crown-mode-add")
        .w_full()
        .icon(IconName::Plus)
        .label(tr!("Add mode"))
        .on_click(|_event, _window, cx| {
            AppState::update_bindings(cx, |state| {
                let name = state
                    .next_crown_mode_name(|n| tr!("Mode %{n}", n => n.to_string()).to_string());
                state.add_crown_mode(name);
            });
        })
}
