use vault_protocol::{EVENT_BYTES, RawEvent};
use vst3::Steinberg::Vst::Event_::EventTypes_;
use vst3::Steinberg::Vst::{Event, Event__type0, NoteOffEvent, NoteOnEvent};

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

fn note(on: bool, sample_offset: i32, pitch: i16, velocity: f32) -> Event {
    let mut event: Event = unsafe { std::mem::zeroed() };
    event.sampleOffset = sample_offset;
    if on {
        event.r#type = EventTypes_::kNoteOnEvent as u16;
        event.__field0 = Event__type0 {
            noteOn: NoteOnEvent { channel: 0, pitch, tuning: 0.0, velocity, length: 0, noteId: -1 },
        };
    } else {
        event.r#type = EventTypes_::kNoteOffEvent as u16;
        event.__field0 = Event__type0 { noteOff: NoteOffEvent { channel: 0, pitch, velocity, noteId: -1, tuning: 0.0 } };
    }
    event
}

pub fn note_on(sample_offset: i32, pitch: i16, velocity: f32) -> Event {
    note(true, sample_offset, pitch, velocity)
}

pub fn note_off(sample_offset: i32, pitch: i16) -> Event {
    note(false, sample_offset, pitch, 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_survives_the_trip() {
        let sent = note_on(17, 60, 0.75);
        let received = from_raw(&to_raw(&sent).expect("a note travels"));
        assert_eq!((received.sampleOffset, received.r#type), (17, sent.r#type));
        let (pitch, velocity) = unsafe { (received.__field0.noteOn.pitch, received.__field0.noteOn.velocity) };
        assert_eq!((pitch, velocity), (60, 0.75));
    }

    #[test]
    fn an_event_holding_a_pointer_stays_behind() {
        let mut sysex = note_on(0, 60, 1.0);
        sysex.r#type = EventTypes_::kDataEvent as u16;
        assert!(to_raw(&sysex).is_none());
    }
}
