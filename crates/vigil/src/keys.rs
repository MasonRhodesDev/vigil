use vigil_core::InputEvent;

const VT_FIRST: u32 = 0x1008_FE01;
const VT_LAST: u32 = 0x1008_FE0C;
const ESCAPE: u32 = 0xff1b;

pub trait KeySink {
    fn change_vt(&mut self, vt: i32);
    fn cancel(&mut self);
    fn forward(&mut self, event: InputEvent);
}

pub fn route(sink: &mut impl KeySink, event: InputEvent) {
    let InputEvent::Key {
        keysym, pressed, ..
    } = event
    else {
        return;
    };
    if pressed && (VT_FIRST..=VT_LAST).contains(&keysym) {
        sink.change_vt((keysym - VT_FIRST + 1) as i32);
        return;
    }
    if pressed && keysym == ESCAPE {
        sink.cancel();
    }
    sink.forward(event);
}
