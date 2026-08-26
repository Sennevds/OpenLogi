//! Live key capture for one keyboard: divert the bound F-row controls over
//! HID++ `0x1b04` — and, on a Craft, the crown dial over `0x4600` — and turn
//! their physical edges into [`CapturedInput`] the agent can dispatch.
//!
//! [`run_keyboard_capture_session`] is the keyboard counterpart of
//! [`crate::session::gesture::run_capture_session`]: one open channel, diversion armed
//! on exactly the controls the caller asks for (an unbound key is never
//! diverted, so it keeps its native firmware function), one message listener,
//! and every diverted control handed back to the firmware on shutdown.
//!
//! Diversion works on the key's *control* — the printed media/shortcut
//! function — so it fires when Fn-lock is off (or via Fn+key when it is on).
//! The plain F1–F12 codes of an Fn-locked row travel the ordinary HID keyboard
//! interface and never reach `0x1b04`.
//!
//! The crown rides along in this session rather than one of its own: a second
//! channel to the same keyboard would split its input-report stream. Both
//! diversions are gated the same way — an unbound control is never diverted,
//! so it keeps its native firmware function (volume, for the crown).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use hidpp::{
    device::Device,
    feature::{
        CreatableFeature, EmittingFeature,
        crown::CrownEvent,
        wireless_device_status::{WirelessDeviceStatusEvent, WirelessDeviceStatusFeature},
    },
    protocol::v20,
};
use openlogi_core::binding::ButtonId;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use super::crown::CrownCapture;
use super::gesture::{CaptureChannel, CapturedInput, GestureError, enumerate_controls, restore};
use crate::ChannelRegistry;
use crate::SharedChannel;
use crate::backend::HidBackend;
use crate::channel::route::{DeviceRoute, open_route_channel};

use crate::reprog_controls::{self, RawControlEvent, ReprogControlsV4};

/// The divertable keyboard F-row controls OpenLogi models, as
/// `(0x1b04 control ID, ButtonId)` pairs. CID values match Logitech's control
/// catalog (cross-checked against Solaar's `special_keys.py`); the F-row
/// positions are the Signature-series layout.
pub const KEYBOARD_KEY_CIDS: [(u16, ButtonId); 9] = [
    (0x00d4, ButtonId::KeySearch),
    (0x0103, ButtonId::KeyDictation),
    (0x0108, ButtonId::KeyEmoji),
    (0x010a, ButtonId::KeyScreenCapture),
    (0x011c, ButtonId::KeyMicMute),
    (0x00e5, ButtonId::KeyPlayPause),
    (0x00e7, ButtonId::KeyMute),
    (0x00e8, ButtonId::KeyVolumeDown),
    (0x00e9, ButtonId::KeyVolumeUp),
];

/// An event one capture session waits on, from either of its two optional
/// sources, so the wait stays a single `select!`.
enum SessionEvent {
    /// The keyboard announced a reconnect — diversion must be re-armed.
    Wake(WirelessDeviceStatusEvent),
    /// The crown reported rotation, touch or button state.
    Crown(CrownEvent),
}

/// What one keyboard capture session should divert.
///
/// Both halves are gated on real bindings by the caller: an unbound control is
/// never diverted, so it keeps its native firmware function.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyboardCaptureTargets {
    /// `0x1b04` control ID → button, for exactly the bound F-row keys.
    pub keys: BTreeMap<u16, ButtonId>,
    /// Whether to divert the crown dial (`0x4600`). Ignored by a keyboard
    /// that exposes no crown.
    pub crown: bool,
}

impl KeyboardCaptureTargets {
    /// Whether this session would divert anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && !self.crown
    }
}

/// Capture the requested keyboard controls on `route` until `shutdown`
/// resolves, forwarding [`CapturedInput::ButtonDown`] and
/// [`CapturedInput::ButtonUp`] edges to `sink`.
///
/// Controls the device doesn't expose (or can't divert) are skipped with a
/// debug log, so a partially-supported keyboard degrades per control rather
/// than failing whole.
pub async fn run_keyboard_capture_session(
    backend: &dyn HidBackend,
    route: DeviceRoute,
    targets: KeyboardCaptureTargets,
    sink: mpsc::UnboundedSender<CapturedInput>,
    shutdown: oneshot::Receiver<()>,
    channel_slot: CaptureChannel,
) -> Result<(), GestureError> {
    let chan = open_route_channel(backend, &route)
        .await?
        .ok_or(GestureError::DeviceNotFound)?;
    let shared = SharedChannel::new(chan, route.clone());
    run_keyboard_capture_session_on(route, shared, targets, sink, shutdown, channel_slot).await
}

/// Run keyboard capture on the exact channel currently published by `registry`.
///
/// A registry miss returns [`GestureError::DeviceNotFound`] without falling
/// back to route enumeration/opening; the agent watcher retries after a later
/// inventory publication.
pub async fn run_keyboard_capture_session_with_registry(
    route: DeviceRoute,
    targets: KeyboardCaptureTargets,
    sink: mpsc::UnboundedSender<CapturedInput>,
    shutdown: oneshot::Receiver<()>,
    channel_slot: CaptureChannel,
    registry: &ChannelRegistry,
) -> Result<(), GestureError> {
    let shared = registry
        .lookup(&route)
        .ok_or(GestureError::DeviceNotFound)?;
    run_keyboard_capture_session_on(route, shared, targets, sink, shutdown, channel_slot).await
}

async fn run_keyboard_capture_session_on(
    route: DeviceRoute,
    shared: SharedChannel,
    targets: KeyboardCaptureTargets,
    sink: mpsc::UnboundedSender<CapturedInput>,
    shutdown: oneshot::Receiver<()>,
    channel_slot: CaptureChannel,
) -> Result<(), GestureError> {
    let chan = Arc::clone(shared.channel());
    let device_index = route.device_index();
    let device = Device::new(Arc::clone(&chan), device_index)
        .await
        .map_err(|_| GestureError::DeviceUnreachable(device_index))?;

    let keys = arm_reprog_keys(&device, &chan, device_index, &targets.keys).await?;

    let listener = keys.as_ref().map(|keys| {
        // Physical press state per CID. Behind a `Mutex` because the channel's
        // read thread invokes the listener by shared reference.
        let held: Arc<Mutex<BTreeSet<u16>>> = Arc::new(Mutex::new(BTreeSet::new()));
        let feature_index = keys.feature_index;
        chan.add_msg_listener_guarded({
            let diverted = keys.diverted.clone();
            let sink = sink.clone();
            move |raw, matched| {
                if matched {
                    return;
                }
                let msg = v20::Message::from(raw);
                let Some(RawControlEvent::DivertedButtons(cids)) =
                    reprog_controls::decode_event(&msg, device_index, feature_index)
                else {
                    return;
                };
                // Recover the guard even if a prior holder panicked — the
                // critical section is panic-free, so the data is consistent.
                let mut down = held.lock().unwrap_or_else(PoisonError::into_inner);
                emit_button_edges(&mut down, &cids, &diverted, &sink);
            }
        })
    });

    // The crown's own feature carries its divert switch and its events, so it
    // needs no message listener here — `CrownCapture` subscribes typed.
    let crown = if targets.crown {
        CrownCapture::arm(&device, &chan, device_index).await?
    } else {
        None
    };

    // Wireless keyboards drop their diverted-control state when they
    // power-cycle (idle sleep, power switch, Easy-Switch host change) — the
    // reconnection broadcast on `0x1d4b` is the firmware asking the host to
    // reconfigure. Re-arm the diversion on every broadcast, or the bound keys
    // silently revert to their native functions after the first nap.
    let wireless = device
        .root()
        .get_feature(WirelessDeviceStatusFeature::ID)
        .await
        .ok()
        .flatten()
        .map(|info| WirelessDeviceStatusFeature::new(Arc::clone(&chan), device_index, info.index));
    let wake_events = wireless.as_ref().map(EmittingFeature::listen);

    // Publish this keyboard's open channel so hardware writes (Fn-lock)
    // reuse it instead of opening the same HID node a second time. Cleared
    // on the way out.
    if let Ok(mut slot) = channel_slot.write() {
        *slot = Some(shared);
    }

    info!(
        index = device_index,
        keys = keys.as_ref().map_or(0, |k| k.diverted.len()),
        crown = crown.is_some(),
        wake_rearm = wake_events.is_some(),
        "keyboard key capture active"
    );

    // Both event sources are optional and either can end early (the emitter is
    // dropped with its feature). Forwarding them into one channel keeps the
    // wait in `serve` a single `select!`: a finished source just drops its
    // sender instead of leaving a branch that resolves instantly forever.
    let (events_tx, events) = mpsc::unbounded_channel::<SessionEvent>();
    if let Some(wake_events) = wake_events {
        let tx = events_tx.clone();
        tokio::spawn(async move {
            while let Ok(event) = wake_events.recv().await {
                if tx.send(SessionEvent::Wake(event)).is_err() {
                    break;
                }
            }
        });
    }
    if let Some(crown) = crown.as_ref() {
        crown.forward_events(events_tx.clone(), SessionEvent::Crown);
    }
    // Held by the forwarders only, so `events.recv()` resolves to `None` once
    // every source is gone rather than blocking forever.
    drop(events_tx);

    serve(keys.as_ref(), crown.as_ref(), events, &sink, shutdown).await;

    drop(listener);
    if let Ok(mut slot) = channel_slot.write() {
        *slot = None;
    }
    if let Some(keys) = keys.as_ref() {
        for &cid in keys.diverted.keys() {
            restore(
                keys.rc.set_cid_reporting(cid, false, false).await,
                "keyboard key",
            );
        }
    }
    if let Some(crown) = crown.as_ref() {
        crown.restore().await;
    }
    debug!(index = device_index, "keyboard key capture stopped");
    Ok(())
}

/// The `0x1b04` half of a session: the armed controls and the feature handle
/// needed to decode their events and hand them back on shutdown.
struct DivertedKeys {
    rc: ReprogControlsV4,
    feature_index: u8,
    diverted: BTreeMap<u16, ButtonId>,
}

/// Divert the bound F-row controls, or `Ok(None)` when none are bound.
///
/// The crown does not need `0x1b04`, so a session diverting only the crown
/// must not fail on a keyboard that exposes no reprog controls — hence the
/// whole half being skipped rather than probed and tolerated.
async fn arm_reprog_keys(
    device: &Device,
    chan: &Arc<hidpp::channel::HidppChannel>,
    device_index: u8,
    wanted: &BTreeMap<u16, ButtonId>,
) -> Result<Option<DivertedKeys>, GestureError> {
    if wanted.is_empty() {
        return Ok(None);
    }
    let info = device
        .root()
        .get_feature(reprog_controls::FEATURE_ID)
        .await
        .map_err(|e| GestureError::Hidpp(format!("{e:?}")))?
        .ok_or_else(|| GestureError::Hidpp("keyboard exposes no 0x1b04 reprog controls".into()))?;
    let rc = ReprogControlsV4::new(Arc::clone(chan), device_index, info.index);
    let controls = enumerate_controls(&rc).await?;
    let diverted = arm_keys(&rc, &controls, wanted).await?;
    Ok(Some(DivertedKeys {
        rc,
        feature_index: info.index,
        diverted,
    }))
}

/// Serve the session until `shutdown` resolves: re-arm diversion on every
/// reconnect broadcast, and forward translated crown inputs to `sink`.
async fn serve(
    keys: Option<&DivertedKeys>,
    crown: Option<&CrownCapture>,
    events: mpsc::UnboundedReceiver<SessionEvent>,
    sink: &mpsc::UnboundedSender<CapturedInput>,
    shutdown: oneshot::Receiver<()>,
) {
    let mut events = events;
    let mut translator = crown.map(CrownCapture::translator);
    let mut shutdown = shutdown;
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            event = events.recv() => {
                match event {
                    Some(SessionEvent::Wake(event)) => {
                        let WirelessDeviceStatusEvent::StatusBroadcast(broadcast) = event else {
                            continue;
                        };
                        info!(?broadcast, "keyboard reconnected — re-arming diversion");
                        if let Some(keys) = keys {
                            rearm_keys(&keys.rc, &keys.diverted).await;
                        }
                        if let Some(crown) = crown {
                            crown.rearm().await;
                        }
                    }
                    Some(SessionEvent::Crown(event)) => {
                        let CrownEvent::Update(update) = event else {
                            continue;
                        };
                        if let Some(translator) = translator.as_mut() {
                            for input in translator.update(&update) {
                                let _ = sink.send(input);
                            }
                        }
                    }
                    // Every source ended; only shutdown remains.
                    None => {
                        let _ = (&mut shutdown).await;
                        break;
                    }
                }
            }
        }
    }
}

/// Diff one full diverted-control snapshot into exactly one edge per physical
/// transition. Unchanged snapshots are deliberately silent.
fn emit_button_edges(
    down: &mut BTreeSet<u16>,
    cids: &[u16],
    diverted: &BTreeMap<u16, ButtonId>,
    sink: &mpsc::UnboundedSender<CapturedInput>,
) {
    for (&cid, &button) in diverted {
        let now = cids.contains(&cid);
        let was = down.contains(&cid);
        if now && !was {
            let _ = sink.send(CapturedInput::ButtonDown(button));
        } else if !now && was {
            let _ = sink.send(CapturedInput::ButtonUp(button));
        }
        if now {
            down.insert(cid);
        } else {
            down.remove(&cid);
        }
    }
}

/// Divert every wanted control the keyboard exposes as divertable, returning
/// the armed `CID → ButtonId` subset. Missing / non-divertable controls are
/// skipped with a debug log, so a partially-supported keyboard degrades per
/// key rather than failing whole.
async fn arm_keys(
    rc: &ReprogControlsV4,
    controls: &[reprog_controls::CtrlIdInfo],
    wanted: &BTreeMap<u16, ButtonId>,
) -> Result<BTreeMap<u16, ButtonId>, GestureError> {
    let mut diverted = BTreeMap::new();
    for (&cid, &button) in wanted {
        if controls.iter().any(|c| c.cid == cid && c.is_divertable()) {
            rc.set_cid_reporting(cid, true, false)
                .await
                .map_err(|e| GestureError::Hidpp(format!("{e:?}")))?;
            diverted.insert(cid, button);
        } else {
            debug!(
                cid = format_args!("{cid:#06x}"),
                "bound key not divertable on this keyboard — left native"
            );
        }
    }
    Ok(diverted)
}

/// Re-issue diversion for every armed control after a device power-cycle.
/// Failures are logged, not propagated — the next reconnection broadcast
/// retries.
async fn rearm_keys(rc: &ReprogControlsV4, diverted: &BTreeMap<u16, ButtonId>) {
    // A settling pause: the broadcast arrives the instant the link is back,
    // occasionally before the device accepts feature writes again.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    for &cid in diverted.keys() {
        if let Err(e) = rc.set_cid_reporting(cid, true, false).await {
            warn!(
                cid = format_args!("{cid:#06x}"),
                error = ?e,
                "re-divert after wake failed — key stays native until next wake"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_snapshots_emit_balanced_edges_without_duplicates() {
        let diverted = BTreeMap::from([
            (0x00d4, ButtonId::KeySearch),
            (0x0103, ButtonId::KeyDictation),
        ]);
        let (sink, mut inputs) = mpsc::unbounded_channel();
        let mut down = BTreeSet::new();

        emit_button_edges(&mut down, &[0x00d4], &diverted, &sink);
        emit_button_edges(&mut down, &[0x00d4], &diverted, &sink);
        emit_button_edges(&mut down, &[0x00d4, 0x0103], &diverted, &sink);
        emit_button_edges(&mut down, &[0x0103], &diverted, &sink);
        emit_button_edges(&mut down, &[], &diverted, &sink);

        assert_eq!(
            std::iter::from_fn(|| inputs.try_recv().ok()).collect::<Vec<_>>(),
            vec![
                CapturedInput::ButtonDown(ButtonId::KeySearch),
                CapturedInput::ButtonDown(ButtonId::KeyDictation),
                CapturedInput::ButtonUp(ButtonId::KeySearch),
                CapturedInput::ButtonUp(ButtonId::KeyDictation),
            ]
        );
    }
}
