//! `OPENLOGI_RING_PREVIEW=1`: paint one ring and nothing else.
//!
//! The overlay normally draws only what the agent hands it, so a visual change
//! to the ring cannot be seen without a bound control and a physical press.
//! This is the ring's equivalent of the settings app's
//! `OPENLOGI_COMPONENT_GALLERY=1`: a representative ring, no IPC, no agent, no
//! role tenancy — so it can be run, looked at, and screenshotted for review.
//!
//! Deliberately not a substitute for the real thing: it cannot show hover
//! feedback driven by the agent, or how the ring sits over a *live* desktop
//! under the cursor.

use std::collections::BTreeMap;
use std::sync::Arc;

use gpui::AppContext as _;
use openlogi_core::binding::{ActionRingIcon, ActionRingSlot};
use openlogi_ipc::{ActionRingInvocation, ActionRingPresentation};
use tokio::sync::mpsc;

use crate::platform;
use crate::ring::{RingView, ring_window_options};
use crate::session::ClickAwaySession;

/// One icon per slot, chosen to show the glyph set at its usual variety rather
/// than eight copies of one shape.
const SLOTS: [(ActionRingSlot, ActionRingIcon, &str); 8] = [
    (ActionRingSlot::Top, ActionRingIcon::Cut, "Cut"),
    (ActionRingSlot::TopRight, ActionRingIcon::Copy, "Copy"),
    (ActionRingSlot::Right, ActionRingIcon::Paste, "Paste"),
    (
        ActionRingSlot::BottomRight,
        ActionRingIcon::Search,
        "Search",
    ),
    (
        ActionRingSlot::Bottom,
        ActionRingIcon::Applications,
        "Applications",
    ),
    (
        ActionRingSlot::BottomLeft,
        ActionRingIcon::Keyboard,
        "Keyboard",
    ),
    (ActionRingSlot::Left, ActionRingIcon::Save, "Save"),
    (ActionRingSlot::TopLeft, ActionRingIcon::Grid, "Grid"),
];

fn invocation() -> ActionRingInvocation {
    let slots: BTreeMap<_, _> = SLOTS
        .iter()
        .map(|(slot, icon, label)| {
            (
                *slot,
                ActionRingPresentation {
                    label: (*label).to_owned(),
                    literal: true,
                    icon: *icon,
                },
            )
        })
        .collect();
    ActionRingInvocation {
        session_id: 0,
        slots,
        language: None,
    }
}

/// Open one preview ring and run until the window closes.
pub(crate) fn run() {
    let mut app = gpui_platform::application().with_assets(openlogi_ui::action_icons::ActionIcons);
    app = app.with_quit_mode(gpui::QuitMode::Explicit);
    app.run(move |cx| {
        platform::configure_application();
        let live_session = Arc::new(ClickAwaySession::new());
        // The command sink is drained and dropped: nothing acts on hover or
        // activation here, but `RingView` still reports them.
        let (commands, mut sink) = mpsc::unbounded_channel();
        cx.background_executor()
            .spawn(async move { while sink.recv().await.is_some() {} })
            .detach();
        let options = ring_window_options(cx);
        if let Err(error) = cx.open_window(options, |_, cx| {
            cx.new(|_| RingView::new(invocation(), commands, &live_session))
        }) {
            tracing::warn!(%error, "could not open the preview ring");
        } else {
            // The same native touch-up the real path applies, so the preview
            // shows the window's actual shape.
            platform::configure_windows();
        }
    });
}
