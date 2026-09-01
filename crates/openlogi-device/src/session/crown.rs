//! The Craft keyboard's crown dial, captured over HID++ `0x4600`.
//!
//! The crown is not a `0x1b04` control: it has its own feature, its own
//! divert switch, and an event carrying rotation, touch, gesture and button
//! state in one packet. This module owns that translation — [`CrownCapture`]
//! arms the divert and restores it, and [`CrownTranslator`] turns each
//! [`CrownUpdate`] into the [`CapturedInput`] edges the agent already knows
//! how to dispatch.
//!
//! It is armed from inside [`super::keyboard`]'s session rather than a session
//! of its own: a second channel to the same keyboard would split its
//! input-report stream, so the crown shares the one the F-row keys use.
//!
//! Diverting the crown takes away its native function (volume), so the caller
//! arms it only when one of the crown's controls carries a real binding — the
//! same rule the F-row keys follow.

use std::sync::Arc;

use hidpp::{
    channel::HidppChannel,
    device::Device,
    feature::{
        CreatableFeature, EmittingFeature,
        crown::{
            ActivityState, ButtonState, CrownEvent, CrownFeature, CrownGesture, CrownUpdate,
            RatchetMode, ReportingMode, SetCrownMode,
        },
    },
};
use openlogi_core::binding::ButtonId;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::gesture::{CapturedInput, GestureError};

/// Slots per ratchet detent to assume when the device reports counts we can't
/// divide — a plausible mid-range figure that keeps rotation moving rather
/// than silently swallowing it.
const FALLBACK_SLOTS_PER_DETENT: i32 = 8;

/// Most rotation steps to emit from one event. A fast flick reports a large
/// slot delta; without a ceiling one packet could fire dozens of actions
/// (e.g. dozens of volume steps) from a single physical spin.
const MAX_STEPS_PER_EVENT: i32 = 8;

/// One armed crown: the diverted feature plus the geometry needed to turn its
/// rotation reports into detent steps.
pub(super) struct CrownCapture {
    feature: Arc<CrownFeature>,
    /// Slots the crown reports per ratchet detent, from its own `getInfo`.
    slots_per_detent: i32,
}

impl CrownCapture {
    /// Probe the crown on `device` and divert it.
    ///
    /// Returns `Ok(None)` when the keyboard exposes no `0x4600` feature — a
    /// non-Craft keyboard with bound crown controls degrades to "no crown"
    /// rather than failing the whole session (the F-row keys still capture).
    pub(super) async fn arm(
        device: &Device,
        chan: &Arc<HidppChannel>,
        device_index: u8,
    ) -> Result<Option<Self>, GestureError> {
        let Some(info) = device
            .root()
            .get_feature(CrownFeature::ID)
            .await
            .map_err(|e| GestureError::Hidpp(format!("{e:?}")))?
        else {
            debug!("keyboard exposes no 0x4600 crown — crown bindings inert");
            return Ok(None);
        };

        let feature = Arc::new(CrownFeature::new(
            Arc::clone(chan),
            device_index,
            info.index,
        ));
        // `getInfo` before the divert: the slot/ratchet geometry is what makes
        // a rotation report mean anything, and reading it is harmless if the
        // divert below then fails.
        let slots_per_detent = match feature.get_info().await {
            Ok(info) => slots_per_detent(info.slots, info.ratchets),
            Err(e) => {
                debug!(error = ?e, "crown getInfo failed — assuming default detent size");
                FALLBACK_SLOTS_PER_DETENT
            }
        };

        // Divert rotation to HID++ and leave every other setting alone: the
        // ratchet mode and the three timeouts are the user's (or the
        // firmware's) business, and `NoChange`/`0` are the wire sentinels for
        // "don't touch" — see `SetCrownMode`.
        feature
            .set_mode(SetCrownMode {
                diverting: ReportingMode::Diverted,
                ratchet_mode: RatchetMode::NoChange,
                rotation_timeout: 0,
                short_long_timeout: 0,
                double_tap_speed: 0,
            })
            .await
            .map_err(|e| GestureError::Hidpp(format!("crown divert failed: {e:?}")))?;

        debug!(slots_per_detent, "crown diverted");
        Ok(Some(Self {
            feature,
            slots_per_detent,
        }))
    }

    /// Subscribe to this crown's diverted events and forward each one into
    /// `tx`, wrapped by `wrap`.
    ///
    /// The subscription lives in the spawned task, so the caller never names
    /// the emitter's channel type; it ends on its own when this
    /// [`CrownCapture`] is dropped and the feature takes its emitter with it.
    pub(super) fn forward_events<T: Send + 'static>(
        &self,
        tx: mpsc::UnboundedSender<T>,
        wrap: impl Fn(CrownEvent) -> T + Send + 'static,
    ) {
        let events = EmittingFeature::listen(&*self.feature);
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if tx.send(wrap(event)).is_err() {
                    break;
                }
            }
        });
    }

    /// A translator matched to this crown's detent geometry.
    pub(super) fn translator(&self) -> CrownTranslator {
        CrownTranslator::new(self.slots_per_detent)
    }

    /// Re-divert after the keyboard power-cycled. The crown's mode lives in
    /// device RAM, so a nap hands rotation back to the firmware exactly like
    /// it does the diverted F-row keys.
    pub(super) async fn rearm(&self) {
        if let Err(e) = self
            .feature
            .set_mode(SetCrownMode {
                diverting: ReportingMode::Diverted,
                ratchet_mode: RatchetMode::NoChange,
                rotation_timeout: 0,
                short_long_timeout: 0,
                double_tap_speed: 0,
            })
            .await
        {
            warn!(error = ?e, "crown re-divert after wake failed — crown stays native until next wake");
        }
    }

    /// Hand the crown back to the firmware, restoring its native function.
    pub(super) async fn restore(&self) {
        let result = self
            .feature
            .set_mode(SetCrownMode {
                diverting: ReportingMode::Hid,
                ratchet_mode: RatchetMode::NoChange,
                rotation_timeout: 0,
                short_long_timeout: 0,
                double_tap_speed: 0,
            })
            .await
            .map(|_| ());
        super::gesture::restore(result.map_err(|e| format!("{e:?}")), "crown");
    }
}

/// How many slots make one detent, from the crown's reported per-revolution
/// counts. Guards the division: a crown reporting zero ratchets (or more
/// ratchets than slots) would otherwise divide by zero or by nothing.
fn slots_per_detent(slots: u16, ratchets: u16) -> i32 {
    let slots = i32::from(slots);
    let ratchets = i32::from(ratchets);
    if ratchets <= 0 || slots < ratchets {
        return FALLBACK_SLOTS_PER_DETENT;
    }
    (slots / ratchets).max(1)
}

/// The parts of a [`CrownUpdate`] that can produce an input, lifted into a
/// type this crate owns.
///
/// `CrownUpdate` is `#[non_exhaustive]`, so it cannot be constructed outside
/// `hidpp`; converting at the edge keeps [`CrownTranslator`] a plain state
/// machine that tests can drive directly. The dropped fields are dropped
/// deliberately — see [`CrownTranslator::update`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CrownSample {
    /// Detents rotated since the last event, as the firmware counted them.
    ratchets: i8,
    /// Slots rotated since the last event (finer than a detent).
    slots: i8,
    /// Button state at this event.
    button: ButtonState,
    /// Whether this event reports a deliberate single tap.
    tap: bool,
    /// The finger-contact phase, which brackets one touch of the dial.
    touch: TouchPhase,
}

/// One contact's lifecycle, from the crown's touch sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TouchPhase {
    /// No finger, or this crown has no touch sensor.
    #[default]
    Idle,
    /// A finger just arrived.
    Start,
    /// A finger is still on the dial.
    Active,
    /// The finger just left.
    Stop,
}

impl From<ActivityState> for TouchPhase {
    fn from(state: ActivityState) -> Self {
        match state {
            ActivityState::Start => Self::Start,
            ActivityState::Active => Self::Active,
            ActivityState::Stop => Self::Stop,
            // `Inactive` and any value outside the spec mean "no contact to
            // track"; guessing otherwise would strand a contact open.
            _ => Self::Idle,
        }
    }
}

impl From<&CrownUpdate> for CrownSample {
    fn from(u: &CrownUpdate) -> Self {
        Self {
            ratchets: u.relative_ratchet_rotation,
            slots: u.relative_slot_rotation,
            button: u.button,
            tap: u.gesture == CrownGesture::Tap,
            touch: TouchPhase::from(u.touch),
        }
    }
}

/// Turns the crown's continuous rotation and edge-less state packets into
/// discrete [`CapturedInput`]s.
///
/// Rotation is quantised to detents so one physical click of the dial is one
/// action, and press state is diffed so the runtime sees exactly one down and
/// one up per physical press (its own long-press threshold then applies, as
/// for any other diverted button).
pub(super) struct CrownTranslator {
    slots_per_detent: i32,
    /// Sub-detent rotation carried between events, so a slow turn still
    /// accumulates to a step instead of being rounded away each packet.
    residue: i32,
    /// Whether the crown's button is currently held, to emit balanced edges.
    pressed: bool,
    /// The finger contact in progress, if the crown reports touch phases.
    contact: Option<Contact>,
}

/// One finger contact, from touch-start to touch-stop.
///
/// A tap is only a tap if *nothing else* happened while the finger was down,
/// which is what this accumulates. Bracketing the whole contact — rather than
/// judging each packet — is what makes a press-then-release, or a turn, or a
/// finger resting, reliably not-a-tap: the disqualification survives until the
/// finger actually leaves.
#[derive(Debug, Clone, Copy, Default)]
struct Contact {
    /// The firmware called this contact a tap at some point during it.
    tap_seen: bool,
    /// Something happened that a tap excludes: rotation, or a button press.
    disqualified: bool,
}

impl CrownTranslator {
    fn new(slots_per_detent: i32) -> Self {
        Self {
            slots_per_detent,
            residue: 0,
            pressed: false,
            contact: None,
        }
    }

    /// Translate one crown event into zero or more inputs to dispatch.
    ///
    /// Deliberately ignores the proximity and touch *phases*: those fire from
    /// a hand merely resting near the dial, and only the discrete tap gesture
    /// is a deliberate act. The double-tap gesture is ignored too — it has no
    /// separate binding, and the firmware reports it alongside the tap that
    /// began it, so acting on both would fire the tap's action twice.
    pub(super) fn update(&mut self, update: &CrownUpdate) -> Vec<CapturedInput> {
        self.sample(CrownSample::from(update))
    }

    fn sample(&mut self, sample: CrownSample) -> Vec<CapturedInput> {
        let mut out = Vec::new();
        // Whether the button is involved in this event, captured before the
        // press edges below mutate `pressed`. See the tap decision at the end
        // of this function.
        let button_involved = self.pressed || !matches!(sample.button, ButtonState::Inactive);

        // Open a contact the moment the finger lands, so everything below can
        // disqualify it.
        if sample.touch == TouchPhase::Start {
            self.contact = Some(Contact::default());
        }

        let steps = self.rotation_steps(sample);
        let direction = if steps > 0 {
            ButtonId::CrownRotateUp
        } else {
            ButtonId::CrownRotateDown
        };
        for _ in 0..steps.abs() {
            out.push(CapturedInput::ButtonPulse(direction));
        }

        // Press edges. `Press` is the physical transition; the `*Active`
        // states are the firmware repeating "still held", which must not
        // re-fire a down edge.
        match sample.button {
            ButtonState::Press | ButtonState::LongPress if !self.pressed => {
                self.pressed = true;
                out.push(CapturedInput::ButtonDown(ButtonId::CrownPress));
            }
            ButtonState::Release | ButtonState::Inactive if self.pressed => {
                self.pressed = false;
                out.push(CapturedInput::ButtonUp(ButtonId::CrownPress));
            }
            _ => {}
        }

        // The tap decision. A tap is the *absence* of everything else during
        // one finger contact: pressing the crown necessarily touches the
        // capacitive sensor above it, and turning it does too, so the firmware
        // reports its tap gesture alongside both. Judging the whole contact
        // rather than the packet is what makes a press, a turn, or a resting
        // finger reliably not-a-tap — including when the gesture arrives a
        // packet after the press has already gone idle.
        if let Some(contact) = self.contact.as_mut() {
            contact.tap_seen |= sample.tap;
            contact.disqualified |= button_involved || steps != 0;
            // Only the finger leaving settles it.
            if sample.touch == TouchPhase::Stop {
                let tapped = contact.tap_seen && !contact.disqualified;
                self.contact = None;
                if tapped {
                    out.push(CapturedInput::ButtonPulse(ButtonId::CrownTap));
                }
            }
        } else if sample.tap && !button_involved {
            // No contact to bracket: either this crown has no touch sensor, or
            // the gesture arrived without one. Fall back to the per-packet
            // rule so a tap still works rather than being lost entirely.
            out.push(CapturedInput::ButtonPulse(ButtonId::CrownTap));
        }

        out
    }

    /// Detent steps this event completes, signed, clamped to
    /// [`MAX_STEPS_PER_EVENT`]. Consumes the accumulated residue.
    fn rotation_steps(&mut self, sample: CrownSample) -> i32 {
        // Prefer the firmware's own ratchet count — in ratchet mode it is
        // already the detent count, so no accumulation can drift. Free-spin
        // mode reports slots only, which is what the residue is for.
        let steps = if sample.ratchets != 0 {
            self.residue = 0;
            i32::from(sample.ratchets)
        } else if sample.slots != 0 {
            self.residue += i32::from(sample.slots);
            let steps = self.residue / self.slots_per_detent;
            self.residue -= steps * self.slots_per_detent;
            steps
        } else {
            0
        };
        steps.clamp(-MAX_STEPS_PER_EVENT, MAX_STEPS_PER_EVENT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An idle packet: no rotation, no press, no gesture, no contact.
    fn idle() -> CrownSample {
        CrownSample {
            ratchets: 0,
            slots: 0,
            button: ButtonState::Inactive,
            tap: false,
            touch: TouchPhase::Idle,
        }
    }

    /// One deliberate tap as the Craft reports it: the finger lands, the
    /// firmware calls it a tap, the finger leaves.
    fn tap_contact(t: &mut CrownTranslator) -> Vec<CapturedInput> {
        let mut out = t.sample(CrownSample {
            touch: TouchPhase::Start,
            ..idle()
        });
        out.extend(t.sample(CrownSample {
            touch: TouchPhase::Active,
            tap: true,
            ..idle()
        }));
        out.extend(t.sample(CrownSample {
            touch: TouchPhase::Stop,
            ..idle()
        }));
        out
    }

    #[test]
    fn ratchet_reports_map_one_detent_to_one_step() {
        let mut t = CrownTranslator::new(8);
        assert_eq!(
            t.sample(CrownSample {
                ratchets: 1,
                ..idle()
            }),
            vec![CapturedInput::ButtonPulse(ButtonId::CrownRotateUp)]
        );
        assert_eq!(
            t.sample(CrownSample {
                ratchets: -2,
                ..idle()
            }),
            vec![
                CapturedInput::ButtonPulse(ButtonId::CrownRotateDown),
                CapturedInput::ButtonPulse(ButtonId::CrownRotateDown),
            ]
        );
    }

    #[test]
    fn slow_free_spin_accumulates_across_events_instead_of_rounding_away() {
        let mut t = CrownTranslator::new(4);
        let one_slot = CrownSample { slots: 1, ..idle() };

        // Three slots of a four-slot detent: no step yet, but nothing lost.
        for _ in 0..3 {
            assert!(t.sample(one_slot).is_empty());
        }
        assert_eq!(
            t.sample(one_slot),
            vec![CapturedInput::ButtonPulse(ButtonId::CrownRotateUp)],
            "the fourth slot completes the detent"
        );
        assert!(
            t.sample(one_slot).is_empty(),
            "the residue resets after a step, so the next detent needs four more slots"
        );
    }

    #[test]
    fn reversing_direction_does_not_bank_a_step_from_opposite_travel() {
        let mut t = CrownTranslator::new(4);
        assert!(t.sample(CrownSample { slots: 3, ..idle() }).is_empty());
        // Turning back cancels the banked travel rather than completing a
        // detent in the new direction.
        assert!(
            t.sample(CrownSample {
                slots: -3,
                ..idle()
            })
            .is_empty()
        );
    }

    #[test]
    fn a_ratchet_report_discards_stale_sub_detent_travel() {
        let mut t = CrownTranslator::new(4);
        assert!(t.sample(CrownSample { slots: 3, ..idle() }).is_empty());
        // The firmware's own detent count is authoritative; the banked slots
        // must not add a phantom extra step on top of it.
        assert_eq!(
            t.sample(CrownSample {
                ratchets: 1,
                slots: 1,
                ..idle()
            }),
            vec![CapturedInput::ButtonPulse(ButtonId::CrownRotateUp)]
        );
    }

    #[test]
    fn a_fast_flick_is_capped_rather_than_firing_an_unbounded_burst() {
        let mut t = CrownTranslator::new(1);
        assert_eq!(
            t.sample(CrownSample {
                slots: 127,
                ..idle()
            })
            .len(),
            usize::try_from(MAX_STEPS_PER_EVENT).expect("the cap is positive"),
            "one packet must not fire an unbounded action burst"
        );
    }

    #[test]
    fn press_emits_one_balanced_edge_pair_despite_repeated_active_states() {
        let mut t = CrownTranslator::new(8);

        assert_eq!(
            t.sample(CrownSample {
                button: ButtonState::Press,
                ..idle()
            }),
            vec![CapturedInput::ButtonDown(ButtonId::CrownPress)]
        );
        // The firmware repeats "still held" — no second down edge.
        for held in [ButtonState::ShortPressActive, ButtonState::LongPressActive] {
            assert!(
                t.sample(CrownSample {
                    button: held,
                    ..idle()
                })
                .is_empty(),
                "{held:?} must not re-fire a down edge"
            );
        }

        assert_eq!(
            t.sample(CrownSample {
                button: ButtonState::Release,
                ..idle()
            }),
            vec![CapturedInput::ButtonUp(ButtonId::CrownPress)]
        );
        // And no second up edge once released.
        assert!(t.sample(idle()).is_empty());
    }

    #[test]
    fn a_press_that_is_only_ever_reported_as_long_still_balances() {
        // A packet can arrive with the press already promoted to `LongPress`
        // (the `Press` edge lost to a dropped report). It must still open and
        // close exactly one hold, or the runtime keeps a key held forever.
        let mut t = CrownTranslator::new(8);
        assert_eq!(
            t.sample(CrownSample {
                button: ButtonState::LongPress,
                ..idle()
            }),
            vec![CapturedInput::ButtonDown(ButtonId::CrownPress)]
        );
        assert_eq!(
            t.sample(CrownSample {
                button: ButtonState::Release,
                ..idle()
            }),
            vec![CapturedInput::ButtonUp(ButtonId::CrownPress)]
        );
    }

    #[test]
    fn rotating_while_held_does_not_disturb_the_hold() {
        let mut t = CrownTranslator::new(8);
        let _ = t.sample(CrownSample {
            button: ButtonState::Press,
            ..idle()
        });
        assert_eq!(
            t.sample(CrownSample {
                ratchets: 1,
                button: ButtonState::ShortPressActive,
                ..idle()
            }),
            vec![CapturedInput::ButtonPulse(ButtonId::CrownRotateUp)],
            "press-and-turn rotates without re-firing the press"
        );
    }

    #[test]
    fn touch_and_proximity_alone_dispatch_nothing() {
        // Proximity/touch phases are not part of `CrownSample` at all, so a
        // packet carrying only those is idle by construction — this pins the
        // decision that a hand resting on the dial is not an input.
        let mut t = CrownTranslator::new(8);
        assert!(t.sample(idle()).is_empty());
    }

    #[test]
    fn a_tap_pulses_and_a_double_tap_does_not_double_fire() {
        let mut t = CrownTranslator::new(8);
        assert_eq!(
            t.sample(CrownSample {
                tap: true,
                ..idle()
            }),
            vec![CapturedInput::ButtonPulse(ButtonId::CrownTap)]
        );
        // `CrownGesture::DoubleTap` converts to `tap: false` — the firmware
        // reports it alongside the tap that began it.
        assert!(t.sample(idle()).is_empty());
    }

    /// The grammar the whole contact rule exists for: a brief touch with
    /// nothing else in it is a tap.
    #[test]
    fn a_bare_contact_the_firmware_calls_a_tap_pulses_once() {
        let mut t = CrownTranslator::new(8);
        assert_eq!(
            tap_contact(&mut t),
            vec![CapturedInput::ButtonPulse(ButtonId::CrownTap)]
        );
    }

    /// Turning while touching is a turn, not a tap — even though the firmware
    /// reports its tap gesture during the same contact.
    #[test]
    fn a_contact_that_rotates_is_not_a_tap() {
        let mut t = CrownTranslator::new(8);
        t.sample(CrownSample {
            touch: TouchPhase::Start,
            ..idle()
        });
        let turned = t.sample(CrownSample {
            touch: TouchPhase::Active,
            ratchets: 1,
            tap: true,
            ..idle()
        });
        assert_eq!(
            turned,
            vec![CapturedInput::ButtonPulse(ButtonId::CrownRotateUp)],
            "the rotation dispatches, the tap does not"
        );
        assert!(
            t.sample(CrownSample {
                touch: TouchPhase::Stop,
                ..idle()
            })
            .is_empty(),
            "lifting off after a turn must not fire the tap"
        );
    }

    /// Pushing while touching is a press, not a tap.
    #[test]
    fn a_contact_that_presses_is_not_a_tap() {
        let mut t = CrownTranslator::new(8);
        t.sample(CrownSample {
            touch: TouchPhase::Start,
            ..idle()
        });
        t.sample(CrownSample {
            touch: TouchPhase::Active,
            button: ButtonState::Press,
            tap: true,
            ..idle()
        });
        let released = t.sample(CrownSample {
            touch: TouchPhase::Active,
            button: ButtonState::Release,
            ..idle()
        });
        assert_eq!(
            released,
            vec![CapturedInput::ButtonUp(ButtonId::CrownPress)]
        );
        assert!(
            t.sample(CrownSample {
                touch: TouchPhase::Stop,
                ..idle()
            })
            .is_empty(),
            "the press already happened; lifting off is not also a tap"
        );
    }

    /// The case the per-packet rule could not catch: the gesture arriving
    /// after the button has gone idle, while the finger is still down.
    #[test]
    fn a_tap_reported_after_a_press_within_one_contact_is_still_suppressed() {
        let mut t = CrownTranslator::new(8);
        t.sample(CrownSample {
            touch: TouchPhase::Start,
            button: ButtonState::Press,
            ..idle()
        });
        t.sample(CrownSample {
            touch: TouchPhase::Active,
            button: ButtonState::Release,
            ..idle()
        });
        // Button idle now, but the finger has not left.
        t.sample(CrownSample {
            touch: TouchPhase::Active,
            tap: true,
            ..idle()
        });
        assert!(
            t.sample(CrownSample {
                touch: TouchPhase::Stop,
                ..idle()
            })
            .is_empty(),
            "one contact, one verdict — the press disqualified it"
        );
    }

    /// A finger resting on the dial is not a tap, however long it rests.
    #[test]
    fn a_contact_the_firmware_never_calls_a_tap_pulses_nothing() {
        let mut t = CrownTranslator::new(8);
        t.sample(CrownSample {
            touch: TouchPhase::Start,
            ..idle()
        });
        for _ in 0..20 {
            assert!(
                t.sample(CrownSample {
                    touch: TouchPhase::Active,
                    ..idle()
                })
                .is_empty()
            );
        }
        assert!(
            t.sample(CrownSample {
                touch: TouchPhase::Stop,
                ..idle()
            })
            .is_empty()
        );
    }

    /// Consecutive taps each stand alone: a disqualified contact must not
    /// poison the next one.
    #[test]
    fn a_disqualified_contact_does_not_poison_the_next_tap() {
        let mut t = CrownTranslator::new(8);
        t.sample(CrownSample {
            touch: TouchPhase::Start,
            ..idle()
        });
        t.sample(CrownSample {
            touch: TouchPhase::Active,
            ratchets: 1,
            tap: true,
            ..idle()
        });
        t.sample(CrownSample {
            touch: TouchPhase::Stop,
            ..idle()
        });
        assert_eq!(
            tap_contact(&mut t),
            vec![CapturedInput::ButtonPulse(ButtonId::CrownTap)],
            "the next clean contact taps"
        );
    }

    /// A crown with no touch sensor reports no phases; the tap must still
    /// work rather than being lost with the contact that never opens.
    #[test]
    fn a_crown_without_touch_phases_falls_back_to_the_packet_rule() {
        let mut t = CrownTranslator::new(8);
        assert_eq!(
            t.sample(CrownSample {
                tap: true,
                ..idle()
            }),
            vec![CapturedInput::ButtonPulse(ButtonId::CrownTap)]
        );
    }

    /// Pressing the crown touches its capacitive sensor, so the firmware
    /// reports a tap in the same packet. Dispatching both made one press run
    /// the press action *and* the tap action — which, with the tap bound to
    /// the mode cycle, meant clicking the dial changed mode.
    #[test]
    fn a_press_that_also_reports_a_tap_does_not_pulse_the_tap() {
        let mut t = CrownTranslator::new(8);
        assert_eq!(
            t.sample(CrownSample {
                button: ButtonState::Press,
                tap: true,
                ..idle()
            }),
            vec![CapturedInput::ButtonDown(ButtonId::CrownPress)],
            "the press is the event; the tap is its finger"
        );
    }

    /// The tap can also arrive while the press is still being held, or as the
    /// finger lifts off.
    #[test]
    fn a_tap_while_held_or_releasing_is_suppressed() {
        let mut t = CrownTranslator::new(8);
        t.sample(CrownSample {
            button: ButtonState::Press,
            ..idle()
        });
        assert!(
            t.sample(CrownSample {
                button: ButtonState::LongPressActive,
                tap: true,
                ..idle()
            })
            .is_empty(),
            "a tap mid-hold is the held finger"
        );
        assert_eq!(
            t.sample(CrownSample {
                button: ButtonState::Release,
                tap: true,
                ..idle()
            }),
            vec![CapturedInput::ButtonUp(ButtonId::CrownPress)],
            "a tap on release is the finger leaving"
        );
    }

    /// The whole point of the suppression is that it is narrow: a touch tap
    /// with no button activity is still a tap.
    #[test]
    fn a_tap_after_the_button_goes_idle_still_pulses() {
        let mut t = CrownTranslator::new(8);
        t.sample(CrownSample {
            button: ButtonState::Press,
            ..idle()
        });
        t.sample(CrownSample {
            button: ButtonState::Release,
            ..idle()
        });
        assert_eq!(
            t.sample(CrownSample {
                tap: true,
                ..idle()
            }),
            vec![CapturedInput::ButtonPulse(ButtonId::CrownTap)]
        );
    }

    #[test]
    fn rotation_and_press_in_one_packet_both_dispatch() {
        let mut t = CrownTranslator::new(8);
        assert_eq!(
            t.sample(CrownSample {
                ratchets: 1,
                button: ButtonState::Press,
                ..idle()
            }),
            vec![
                CapturedInput::ButtonPulse(ButtonId::CrownRotateUp),
                CapturedInput::ButtonDown(ButtonId::CrownPress),
            ]
        );
    }

    #[test]
    fn degenerate_geometry_falls_back_instead_of_dividing_by_zero() {
        assert_eq!(slots_per_detent(0, 0), FALLBACK_SLOTS_PER_DETENT);
        assert_eq!(slots_per_detent(24, 0), FALLBACK_SLOTS_PER_DETENT);
        assert_eq!(
            slots_per_detent(8, 24),
            FALLBACK_SLOTS_PER_DETENT,
            "more ratchets than slots is not a geometry we can divide"
        );
        assert_eq!(slots_per_detent(24, 24), 1);
        assert_eq!(slots_per_detent(96, 24), 4);
    }
}
