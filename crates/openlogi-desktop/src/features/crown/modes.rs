//! The crown-mode list: what a tap cycles the dial through.
//!
//! One card per mode, each carrying the three controls a mode may own. The tap
//! is deliberately absent from that set — it is what cycles modes, so a mode
//! that could rebind it would strand the user inside itself.
//!
//! Rendering only. The list belongs to whichever profile
//! `AppState::editing_app` has open, and every edit goes through `AppState`'s
//! crown-mode commits, so this module never touches config directly.

use gpui::{
    AppContext as _, Entity, IntoElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement as _, Styled, Window, div, prelude::FluentBuilder as _, px, svg,
};
use gpui_component::{
    Icon, IconName, Selectable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogButtonProps,
    h_flex,
    input::InputState,
    v_flex,
};
use openlogi_core::binding::{Action, ButtonId, CrownMode};

use super::{CrownPanel, LIST_W, Target};
use crate::features::mouse::picker::{action_icon_path, compact_panel, divider};
use crate::state::AppState;
use crate::ui::action::localized_action_label;
use crate::ui::components::{MenuRow, control_button, control_input};
use crate::ui::section::section_label;
use crate::ui::theme::{Palette, Typography as _};

/// The controls a mode may own, in the order the cards list them.
pub(super) const MODE_CONTROLS: [ButtonId; 3] = [
    ButtonId::CrownRotateUp,
    ButtonId::CrownRotateDown,
    ButtonId::CrownPress,
];

/// The whole modes section: heading, one card per mode, and the add button.
pub(super) fn modes_section(
    modes: &[CrownMode],
    cycles_modes: bool,
    overridden: bool,
    selected: Option<Target>,
    view: &Entity<CrownPanel>,
    pal: Palette,
) -> gpui::Div {
    v_flex()
        .gap_2()
        .w(px(LIST_W))
        .child(heading(modes, cycles_modes, pal))
        .children(
            modes
                .iter()
                .enumerate()
                .map(|(index, mode)| mode_card(index, mode, selected, view, pal)),
        )
        .when(modes.is_empty(), |section| {
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
        .child(add_mode_button())
        .when(overridden, |section| {
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

/// Section heading, plus the one warning worth surfacing: modes nobody can
/// reach. A list no control cycles is configuration with no effect, and the fix
/// is a control binding one panel above — so say so, rather than leaving the
/// user to discover the dial ignoring them.
fn heading(modes: &[CrownMode], cycles_modes: bool, pal: Palette) -> gpui::Div {
    let unreachable = !modes.is_empty() && !cycles_modes;
    v_flex()
        .w_full()
        .gap_0p5()
        .child(section_label(tr!("Modes"), pal).w_full())
        .child(
            div()
                .text_caption()
                .text_color(if unreachable {
                    pal.text_primary
                } else {
                    pal.text_muted
                })
                .child(if unreachable {
                    tr!("Bind a control to Cycle Crown Mode to reach these.")
                } else {
                    tr!("Cycled by whichever control runs Cycle Crown Mode.")
                }),
        )
}

/// One mode: its name, the rename and remove buttons, and its three controls.
fn mode_card(
    index: usize,
    mode: &CrownMode,
    selected: Option<Target>,
    view: &Entity<CrownPanel>,
    pal: Palette,
) -> gpui::Div {
    compact_panel(pal)
        .w_full()
        .child(card_header(index, mode, pal))
        .child(divider(pal))
        .children(MODE_CONTROLS.iter().copied().map(|control| {
            mode_control_row(
                index,
                control,
                mode.action_for(control),
                selected == Some(Target::ModeControl(index, control)),
                view,
                pal,
            )
        }))
}

fn card_header(index: usize, mode: &CrownMode, pal: Palette) -> gpui::Div {
    let name = mode.name.clone();
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
        .child(
            Button::new(("crown-mode-rename", index))
                .xsmall()
                .ghost()
                .icon(Icon::empty().path("action-icons/pencil.svg"))
                .on_click(move |_event, window, cx| {
                    let kind = NameDialog::Rename {
                        index,
                        current: name.clone(),
                    };
                    open_name_dialog(kind, window, cx);
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
}

/// One control inside a mode. "Falls through" is the honest label for an unset
/// one: the mode leaves that control on its ordinary binding rather than
/// disabling it.
fn mode_control_row(
    index: usize,
    control: ButtonId,
    action: Option<&Action>,
    selected: bool,
    view: &Entity<CrownPanel>,
    pal: Palette,
) -> MenuRow {
    let view = view.clone();
    let label = action.map_or_else(|| tr!("Falls through"), localized_action_label);
    // Keyed by mode and control rather than row position, so adding a mode does
    // not move focus between existing rows.
    MenuRow::new((
        "crown-mode-control",
        index * MODE_CONTROLS.len() + control_slot(control),
    ))
    .selected(selected)
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
        view.update(cx, |panel, vcx| {
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

fn add_mode_button() -> impl IntoElement {
    control_button("crown-mode-add")
        .w_full()
        .icon(IconName::Plus)
        .label(tr!("Add mode"))
        .on_click(|_event, window, cx| {
            open_name_dialog(NameDialog::Add, window, cx);
        })
}

/// Which commit a name dialog performs when confirmed.
#[derive(Clone)]
enum NameDialog {
    /// Append a new mode under the entered name.
    Add,
    /// Rename the mode at this index, opening on its current name.
    Rename { index: usize, current: String },
}

impl NameDialog {
    fn title(&self) -> SharedString {
        match self {
            Self::Add => tr!("Add mode"),
            Self::Rename { .. } => tr!("Rename mode"),
        }
    }

    fn initial(&self) -> String {
        match self {
            Self::Add => String::new(),
            Self::Rename { current, .. } => current.clone(),
        }
    }

    fn commit(&self, name: String, cx: &mut gpui::App) {
        let kind = self.clone();
        AppState::update_bindings(cx, move |state| match kind {
            Self::Add => state.add_crown_mode(name),
            Self::Rename { index, .. } => state.rename_crown_mode(index, &name),
        });
    }
}

/// Name a mode. Modelled on the device rename dialog; a blank name is refused
/// by the commit itself, since the name is all the mode indicator can show.
fn open_name_dialog(kind: NameDialog, window: &mut Window, cx: &mut gpui::App) {
    let input = cx.new(|cx| {
        let mut input = InputState::new(window, cx).placeholder(tr!("Volume, Zoom, Tabs…"));
        input.set_value(kind.initial(), window, cx);
        input
    });
    window.open_dialog(cx, move |dialog, window, cx| {
        input.update(cx, |input, cx| input.focus(window, cx));
        dialog
            .w(px(360.))
            .title(kind.title())
            .child(v_flex().gap_2().child(control_input(&input)))
            .button_props(
                DialogButtonProps::default()
                    .ok_text(tr!("Save"))
                    .cancel_text(tr!("Cancel"))
                    .show_cancel(true),
            )
            .on_ok({
                let input = input.clone();
                let kind = kind.clone();
                move |_, _, cx| {
                    kind.commit(input.read(cx).value().to_string(), cx);
                    true
                }
            })
    });
}
