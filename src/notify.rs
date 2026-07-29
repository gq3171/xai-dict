//! Status UI facade for the daemon (Plasma OSD + replaceable notifications).

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

/// Clear sticky status and optionally show a short idle toast.
pub fn idle(msg: &str) {
    osd::hide();
    if !msg.is_empty() {
        osd::boot_hint(msg);
    }
}
