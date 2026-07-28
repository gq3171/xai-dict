//! Status UI: Lazy-style OSD bar for dictation; brief notify only for boot.

use crate::osd;

pub fn recording() {
    osd::recording();
}

pub fn transcribing() {
    osd::transcribing();
}

pub fn done(text: &str) {
    osd::done(text);
}

pub fn error(msg: &str) {
    osd::error(msg);
}

pub fn idle(msg: &str) {
    // Don't leave a bar at idle — short toast once (daemon start).
    osd::hide();
    osd::boot_hint(msg);
}
