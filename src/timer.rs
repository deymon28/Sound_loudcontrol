//! Precise sleeping. `std::thread::sleep` on Windows rounds up to the ~15.6 ms system tick;
//! a high-resolution waitable timer keeps a 10 ms poll interval honest without raising the
//! global timer resolution.

use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::core::PCWSTR;

pub struct Ticker {
    timer: Option<HANDLE>,
}

impl Ticker {
    pub fn new() -> Self {
        let timer = unsafe {
            CreateWaitableTimerExW(None, PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
        };
        match timer {
            Ok(h) => Self { timer: Some(h) },
            Err(e) => {
                log::warn!("high-resolution timer unavailable, falling back to sleep: {e}");
                Self { timer: None }
            }
        }
    }

    pub fn sleep_ms(&mut self, ms: u32) {
        if let Some(h) = self.timer {
            // Negative due time = relative, in 100 ns units.
            let due: i64 = -(ms as i64) * 10_000;
            let armed = unsafe { SetWaitableTimer(h, &due, 0, None, None, false) };
            if armed.is_ok() {
                unsafe { WaitForSingleObject(h, INFINITE) };
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(ms as u64));
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        if let Some(h) = self.timer.take() {
            let _ = unsafe { CloseHandle(h) };
        }
    }
}
