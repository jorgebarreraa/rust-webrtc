#![feature(seek_convenience)]
#![feature(trait_alias)]
#![feature(map_first_last)]
#![feature(seek_stream_len)]

/* for std::intrinsics::breakpoint() */
#![feature(core_intrinsics)]
#![feature(slice_fill)]
#![recursion_limit = "128"]

use crate::srtp2::srtp2_global_init;
use std::sync::{RwLock};
use std::ops::Deref;
use lazy_static::lazy_static;
use crate::transport::crypt::certificate_cache;

pub mod rtc;
pub mod media;
pub mod transport;
pub mod sctp;
pub mod srtp2;
pub mod utils;
pub mod application;

lazy_static! {
    static ref GLOBAL_LOGGER: RwLock<Option<slog::Logger>> = RwLock::new(None);
}

/// Attention:
/// This method must be called within the tokio runtime.
pub fn initialize_webrtc(global_logger: slog::Logger) {
    // unsafe { libnice::sys::nice_debug_enable(1); }
    *GLOBAL_LOGGER.write().unwrap() = Some(global_logger);
    srtp2_global_init().expect("srtp2 init failed");
    openssl::init();

    {
        let mut certificate_cache = certificate_cache().lock().unwrap();
        let init_future = certificate_cache.initialize();
        drop(certificate_cache); /* else a deadlock would occur */

        /* if this fails an error will already be printed */
        let _ = tokio::runtime::Handle::current().block_on(init_future);
    }
}

pub(crate) fn global_logger() -> slog::Logger {
    let locked = GLOBAL_LOGGER.read().unwrap_or_else(|p| p.into_inner());
    if let Some(logger) = locked.deref() {
        logger.clone()
    } else {
        /* Fixme: Use some kind of fallback */
        panic!("missing global logger")
    }
}