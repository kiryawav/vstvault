use vault_protocol::{EVENT_BYTES, RawEvent};
use vst3::Steinberg::Vst::Event;
use vst3::Steinberg::Vst::Event_::EventTypes_;

const _: () = assert!(size_of::<Event>() <= EVENT_BYTES);

pub fn to_raw(event: &Event) -> Option<RawEvent> {
    let plain = [
        EventTypes_::kNoteOnEvent,
        EventTypes_::kNoteOffEvent,
        EventTypes_::kPolyPressureEvent,
        EventTypes_::kNoteExpressionValueEvent,
        EventTypes_::kNoteExpressionIntValueEvent,
        EventTypes_::kLegacyMIDICCOutEvent,
    ];
    if !plain.contains(&i32::from(event.r#type)) {
        return None;
    }
    let mut raw = RawEvent::default();
    unsafe { std::ptr::copy_nonoverlapping((event as *const Event).cast::<u8>(), raw.0.as_mut_ptr(), size_of::<Event>()) };
    Some(raw)
}

pub fn from_raw(raw: &RawEvent) -> Event {
    unsafe { std::ptr::read_unaligned(raw.0.as_ptr().cast::<Event>()) }
}
