//! What `colorama.init(autoreset=True, strip=False)` leaves behind once
//! `ColorizedStyler` has run (`awscli/table.py`): `sys.stdout` and
//! `sys.stderr` are wrappers that write `Style.RESET_ALL` after every
//! write, and `atexit` resets a terminal stdout. The CLI formats output
//! and errors on the driver thread, so the state is kept per thread.

use std::cell::Cell;

thread_local! {
    static INITIALIZED: Cell<bool> = const { Cell::new(false) };
}

pub fn init() {
    INITIALIZED.set(true);
}

pub fn initialized() -> bool {
    INITIALIZED.get()
}

#[cfg(test)]
pub fn reset() {
    INITIALIZED.set(false);
}
