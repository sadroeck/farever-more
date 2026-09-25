//! Local, host-owned chat output.

use crate::__wit::farever::addon::chat as raw;

pub struct Chat {
    _private: (),
}

impl Chat {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self { _private: () }
    }

    pub fn print(&self, text: &str) {
        raw::print(text);
    }

    pub fn error(&self, text: &str) {
        raw::print_error(text);
    }
}
