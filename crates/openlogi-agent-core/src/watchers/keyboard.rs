//! Background HID++ key-capture watcher for a bound keyboard.
//!
//! Runs [`openlogi_hid::run_keyboard_capture_session_with_registry`] on a
//! dedicated thread for the keyboard the orchestrator publishes in
//! [`SharedKeyboardSpec`], restarts it when the keyboard (or the set of bound
//! keys) changes, and dispatches each captured key press through the common
//! action path ([`crate::runtime::ActionDispatcher`]).
//!
//! The mouse capture watcher ([`super::gesture`]) and this one hold *shared*
//! receiver leases, so both run concurrently; pairing still waits for (and
//! excludes) both. Like the gesture watcher, this needs no macOS Accessibility
//! permission — the key events arrive over HID++.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use openlogi_core::binding::{Action, Binding, ButtonId, GestureDirection};
use openlogi_hid::{
    CaptureChannel, CapturedInput, ChannelRegistry, DeviceRoute, KeyboardCaptureTargets,
    run_keyboard_capture_session_with_registry,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::CrownModes;
use crate::receiver_access::ReceiverAccess;
use crate::runtime::{ActionDispatcher, HidppSessionId, PressToken};

/// Everything the watcher needs to capture one keyboard: where it is, which
/// controls to divert (only those carrying a real binding), and the per-key
/// action map presses dispatch through. Rebuilt by the orchestrator on
/// config / inventory / foreground-app changes.
#[derive(Clone)]
pub struct KeyboardSpec {
    /// Live crown-mode state, consulted per event rather than snapshotted:
    /// a tap changes the active mode without any config or inventory change,
    /// so a value captured when this spec was built would be stale by the
    /// time the very next rotation arrives.
    pub crown_modes: Arc<RwLock<CrownModes>>,
    /// Stable config key used to scope lifecycle cancellation and hardware
    /// actions to this keyboard.
    pub config_key: String,
    /// HID++ route of the keyboard.
    pub route: DeviceRoute,
    /// The F-row controls and the crown to divert, for exactly the bound ones.
    pub targets: KeyboardCaptureTargets,
    /// Effective per-key immediate or threshold map (per-app overlay applied).
    pub bindings: BTreeMap<ButtonId, Binding>,
}

/// Shared keyboard-capture spec, `None` when no online keyboard has bound
/// keys. Written by the orchestrator, read by the watcher.
pub type SharedKeyboardSpec = Arc<RwLock<Option<KeyboardSpec>>>;

/// Capture identity excluding bindings, which may change without requiring a
/// hardware session restart when the diverted control set stays the same.
#[derive(Clone, PartialEq)]
struct KeyboardTarget {
    config_key: String,
    route: DeviceRoute,
    targets: KeyboardCaptureTargets,
}

impl KeyboardTarget {
    fn for_spec(spec: KeyboardSpec) -> Self {
        Self {
            config_key: spec.config_key,
            route: spec.route,
            targets: spec.targets,
        }
    }

    fn matches(&self, spec: &KeyboardSpec) -> bool {
        self.config_key == spec.config_key
            && self.route == spec.route
            && self.targets == spec.targets
    }
}

struct RunningKeyboardSession {
    id: HidppSessionId,
    target: KeyboardTarget,
    stop: oneshot::Sender<()>,
}

struct KeyboardInput {
    session: HidppSessionId,
    input: CapturedInput,
}

/// How often to re-read the spec so a config edit, per-app overlay change, or
/// keyboard reconnect re-points the capture session.
const TARGET_POLL: Duration = Duration::from_secs(1);

/// Spawn the keyboard-capture manager thread. It owns a current-thread tokio
/// runtime that keeps one capture session pointed at the bound keyboard and
/// dispatches each captured key press.
pub fn spawn(
    spec: SharedKeyboardSpec,
    keyboard_channel: CaptureChannel,
    receiver_access: ReceiverAccess,
    registry: ChannelRegistry,
    dispatcher: ActionDispatcher,
) {
    thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                warn!(error = %e, "keyboard watcher: could not build tokio runtime");
                return;
            }
        };
        runtime.block_on(manage(
            spec,
            keyboard_channel,
            receiver_access,
            registry,
            dispatcher,
        ));
    });
}

/// The crown press's live lifecycle token, while it is holding a chord.
///
/// Press-and-rotate dispatches each detent *against the press*, the way a
/// raw-XY swipe does, so the direction shares the press's cancellation and
/// generation. Only one keyboard captures at a time and only the crown press
/// chords, so one slot is enough — and a token left behind by a replaced
/// session is harmless: `try_dispatch_while_pressed` rejects it.
#[derive(Default)]
struct CrownChord(Option<PressToken>);

/// Route one accepted keyboard edge through the shared HID++ lifecycle.
fn dispatch_input(
    session: &HidppSessionId,
    input: CapturedInput,
    spec: &KeyboardSpec,
    dispatcher: &ActionDispatcher,
    chord: &mut CrownChord,
) {
    match input {
        CapturedInput::ButtonDown(button) => {
            let effective = effective_binding(spec, button);
            // A crown press carrying a chord map owns its own click slot, so
            // nothing may fire on the down edge: the press is not yet
            // committed to being a click, and a turn is still free to claim
            // it. Same rule — and the same reason — as the raw-XY gesture
            // button, whose click arrives as `Gesture(_, Click)` instead.
            let chorded = is_chord_binding(button, effective.as_ref());
            let binding = if chorded { None } else { effective.as_ref() };
            if let Some(binding) = binding {
                info!(button = %button, action = %binding.click_action().label(), "keyboard key → handling binding");
            } else if chorded {
                debug!(
                    ?button,
                    "crown press holds a chord map — no action on the down edge"
                );
            } else {
                debug!(?button, "keyboard key with no binding — ignored");
            }
            let press = dispatcher.try_hidpp_button_down(session, button, binding);
            if chorded {
                *chord = CrownChord(press);
            }
        }
        CapturedInput::ButtonUp(button) => {
            dispatcher.try_hidpp_button_up(session, button);
            if button == ButtonId::CrownPress {
                *chord = CrownChord::default();
            }
        }
        CapturedInput::ButtonPulse(button) => {
            let binding = effective_binding(spec, button);
            dispatcher.dispatch_hidpp_button_pulse(session, button, binding.as_ref());
        }
        CapturedInput::Gesture(button, direction) => {
            dispatch_crown_chord(session, button, direction, spec, dispatcher, chord);
        }
        CapturedInput::Scroll { .. } => {}
    }
}

/// Dispatch one press-and-rotate direction, or fall back to plain rotation.
///
/// The capture layer reports rotation-while-held as a direction unconditionally
/// — it cannot see bindings. So an unbound chord is resolved *here*, back to
/// the rotation control it would have been, which is what keeps a held dial
/// doing volume for everyone who has not configured a chord.
fn dispatch_crown_chord(
    session: &HidppSessionId,
    button: ButtonId,
    direction: GestureDirection,
    spec: &KeyboardSpec,
    dispatcher: &ActionDispatcher,
    chord: &mut CrownChord,
) {
    let action = effective_binding(spec, button)
        .and_then(|binding| match binding {
            Binding::Gesture(map) => map.get(&direction).cloned(),
            Binding::Single(_) | Binding::LongPress(_) => None,
        })
        .filter(|action| *action != Action::None);

    if let Some(action) = action {
        // Against the press, so the direction inherits its cancellation.
        let Some(press) = chord.0.as_ref() else {
            debug!(
                ?button,
                ?direction,
                "crown chord with no live press — ignored"
            );
            return;
        };
        info!(?button, ?direction, action = %action.label(), "crown chord → action");
        if !dispatcher.try_dispatch_while_pressed(press, &action) {
            debug!(
                ?button,
                ?direction,
                "crown chord press no longer active — ignored"
            );
        }
        return;
    }

    // Unbound. The click slot has nothing to fall back to — the press already
    // dispatched on its own down edge — but a rotation direction does.
    let Some(rotation) = rotation_control(direction) else {
        debug!(?button, ?direction, "crown chord with no binding — ignored");
        return;
    };
    let binding = effective_binding(spec, rotation);
    debug!(
        ?direction,
        ?rotation,
        "crown chord unbound — falling back to plain rotation"
    );
    dispatcher.dispatch_hidpp_button_pulse(session, rotation, binding.as_ref());
}

/// The rotation control a chord direction stands in for, or `None` for the
/// click slot, which is a press and not a rotation.
fn rotation_control(direction: GestureDirection) -> Option<ButtonId> {
    match direction {
        GestureDirection::Up => Some(ButtonId::CrownRotateUp),
        GestureDirection::Down => Some(ButtonId::CrownRotateDown),
        GestureDirection::Left | GestureDirection::Right | GestureDirection::Click => None,
    }
}

/// Whether `binding` makes `button` a chord holder rather than an ordinary
/// press. Only the crown press chords: every other diverted control here is a
/// key, and a key with a gesture map would have no way to produce a direction.
fn is_chord_binding(button: ButtonId, binding: Option<&Binding>) -> bool {
    button == ButtonId::CrownPress && matches!(binding, Some(Binding::Gesture(_)))
}

/// The binding in force for `button`: the active crown mode's override first,
/// then the profile's own.
fn effective_binding(spec: &KeyboardSpec, button: ButtonId) -> Option<Binding> {
    mode_binding(spec, button).or_else(|| spec.bindings.get(&button).cloned())
}

/// The active crown mode's binding for `button`, if a mode claims it.
///
/// `None` means "fall through to the ordinary binding": either this device has
/// no modes, or the active mode leaves this control alone. A mode never claims
/// the tap (see [`openlogi_core::binding::CrownMode::action_for`]), so
/// whatever cycles modes keeps working in every mode.
fn mode_binding(spec: &KeyboardSpec, button: ButtonId) -> Option<Binding> {
    let modes = spec.crown_modes.read().ok()?;
    let action = modes.action_for(Some(&spec.config_key), button)?;
    Some(Binding::Single(action.clone()))
}

/// Snapshot the keyboard session target unless pairing currently owns capture.
fn wanted_session(
    receiver_access: &ReceiverAccess,
    spec: &SharedKeyboardSpec,
) -> Option<KeyboardTarget> {
    if receiver_access.exclusive_requested() {
        return None;
    }
    spec.read()
        .ok()
        .and_then(|guard| guard.clone())
        .map(KeyboardTarget::for_spec)
}

/// The long-lived channels and leases one capture session needs, grouped so
/// starting a session stays one call rather than seven positional arguments.
struct SessionPorts<'a> {
    receiver_access: &'a ReceiverAccess,
    keyboard_channel: &'a CaptureChannel,
    registry: &'a ChannelRegistry,
    /// Where captured inputs are forwarded, tagged with the session id.
    inputs: &'a mpsc::UnboundedSender<KeyboardInput>,
    /// Where the session announces its own completion.
    done: &'a mpsc::UnboundedSender<HidppSessionId>,
}

/// Start one keyboard capture session for `target` at `epoch`.
///
/// `None` when the receiver lease is unavailable — pairing owns it, or the
/// previous session has not let go yet — and the caller simply retries on its
/// next tick.
fn start_session(
    target: KeyboardTarget,
    epoch: u64,
    ports: &SessionPorts<'_>,
) -> Option<RunningKeyboardSession> {
    let receiver_lease = ports.receiver_access.try_acquire_for_session()?;
    let (stop_tx, stop_rx) = oneshot::channel();
    let slot = Arc::clone(ports.keyboard_channel);
    let session_registry = ports.registry.clone();
    let id = HidppSessionId::new(&target.config_key, epoch);

    // Re-tag every input with this session's id, so a stale session's events
    // are recognisable after it has been replaced.
    let (sink, mut session_rx) = mpsc::unbounded_channel();
    let forward = ports.inputs.clone();
    let forward_id = id.clone();
    tokio::spawn(async move {
        while let Some(input) = session_rx.recv().await {
            let _ = forward.send(KeyboardInput {
                session: forward_id.clone(),
                input,
            });
        }
    });

    let done = ports.done.clone();
    let done_id = id.clone();
    let route = target.route.clone();
    let targets = target.targets.clone();
    tokio::spawn(async move {
        // Held for the session's lifetime: dropping the lease here is what
        // lets pairing take the receiver once capture stops.
        let _receiver_lease = receiver_lease;
        if let Err(e) = run_keyboard_capture_session_with_registry(
            route,
            targets,
            sink,
            stop_rx,
            slot,
            &session_registry,
        )
        .await
        {
            debug!(error = %e, "keyboard capture session ended");
        }
        let _ = done.send(done_id);
    });

    Some(RunningKeyboardSession {
        id,
        target,
        stop: stop_tx,
    })
}

/// Keep one keyboard capture session alive for the published spec, restarting
/// it when the keyboard or its bound-key set changes, and dispatch incoming
/// presses. Runs for the lifetime of the process.
async fn manage(
    spec: SharedKeyboardSpec,
    keyboard_channel: CaptureChannel,
    receiver_access: ReceiverAccess,
    registry: ChannelRegistry,
    dispatcher: ActionDispatcher,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<KeyboardInput>();
    let mut current: Option<RunningKeyboardSession> = None;
    // The crown press's chord hold, reset whenever the press ends.
    let mut chord = CrownChord::default();
    let mut ticker = tokio::time::interval(TARGET_POLL);
    // Sessions report completion tagged with their start epoch, so an
    // unexpected exit of the *current* session re-arms while stale completions
    // are ignored — same pacing/starvation reasoning as the gesture watcher.
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<HidppSessionId>();
    let mut epoch: u64 = 0;

    loop {
        tokio::select! {
            Some(input) = rx.recv() => {
                let Some(running) = current.as_ref() else {
                    continue;
                };
                let live_spec = spec.read().ok().and_then(|guard| guard.clone());
                let current_target = live_spec.as_ref().is_some_and(|live| running.target.matches(live));
                if input.session != running.id
                    || receiver_access.exclusive_requested()
                    || !current_target
                {
                    dispatcher.cancel_hidpp_session(&input.session);
                    debug!(epoch = input.session.epoch(), "input from a stale keyboard session — ignored");
                    continue;
                }
                let Some(live_spec) = live_spec else {
                    continue;
                };
                dispatch_input(
                    &input.session,
                    input.input,
                    &live_spec,
                    &dispatcher,
                    &mut chord,
                );
            }
            _ = ticker.tick() => {
                // While pairing is waiting or active, release the capture
                // session so run_pairing can own the receiver's HID node.
                let want = wanted_session(&receiver_access, &spec);
                if current
                    .as_ref()
                    .is_some_and(|running| Some(&running.target) == want.as_ref())
                {
                    continue;
                }
                // Spec changed (or first tick): stop the old session and start
                // one for the new state. Sending on the oneshot lets the old
                // session restore the diverted controls.
                if let Some(running) = current.take() {
                    dispatcher.cancel_hidpp_session(&running.id);
                    let _ = running.stop.send(());
                    continue;
                }
                if let Some(target) = want {
                    epoch = epoch.wrapping_add(1);
                    current = start_session(
                        target,
                        epoch,
                        &SessionPorts {
                            receiver_access: &receiver_access,
                            keyboard_channel: &keyboard_channel,
                            registry: &registry,
                            inputs: &tx,
                            done: &done_tx,
                        },
                    );
                }
            }
            Some(done_session) = done_rx.recv() => {
                // A capture session ended on its own; re-arm only the live one
                // (see gesture watcher for the epoch/pacing rationale).
                if current.as_ref().is_some_and(|running| running.id == done_session) {
                    dispatcher.cancel_hidpp_session(&done_session);
                    warn!("keyboard capture session ended unexpectedly, re-arming");
                    current = None;
                }
            }
        }
    }
}
