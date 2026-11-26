#[path = "lib.rs"]
mod implementation;

pub use implementation::WebHost;

pub(crate) mod web_backend {
    pub(crate) use crate::implementation::*;
}
