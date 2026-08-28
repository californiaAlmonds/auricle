//! Thread scheduling hints that keep playback glitch-free while other apps
//! (notably full-screen games) saturate the CPU.
//!
//! Audio is a hard-real-time workload: the sound card drains its buffer every
//! ~10 ms and the decode thread must refill it in time, every time. A thread at
//! normal priority competing with a game's render/worker threads gets preempted
//! long enough to miss that deadline, which is heard as a click or a short drop
//! out — the "microstutter" symptom.
//!
//! Windows solves this with MMCSS (Multimedia Class Scheduler Service): a thread
//! that registers under the "Pro Audio" task is scheduled in the real-time
//! priority range and is guaranteed a slice even under heavy load. `cpal` 0.15
//! (which `rodio` uses) does *not* register its WASAPI stream thread, so we do it
//! ourselves from inside the decode callback — that code runs *on* the very
//! thread that needs the boost.

#[cfg(target_os = "windows")]
mod imp {
    use std::cell::Cell;
    use windows::core::w;
    use windows::Win32::System::Threading::{
        AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadPriority,
        THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_TIME_CRITICAL,
    };

    thread_local! {
        /// One registration per thread; `register_audio_thread` is called from a
        /// hot path so repeat calls must be nearly free.
        static REGISTERED: Cell<bool> = const { Cell::new(false) };
    }

    /// Promotes the *calling* thread to the MMCSS "Pro Audio" class. Idempotent
    /// per thread and cheap enough to call from the decode path.
    ///
    /// Must be called from the audio callback thread itself, since MMCSS applies
    /// to whichever thread makes the call.
    pub fn register_audio_thread() {
        REGISTERED.with(|done| {
            if done.get() {
                return;
            }
            done.set(true);
            unsafe {
                let mut task_index: u32 = 0;
                match AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) {
                    Ok(handle) if !handle.is_invalid() => {
                        // The handle is deliberately not reverted or closed: the
                        // registration must last as long as the thread, and the
                        // thread belongs to cpal, so there is no later hook where
                        // reverting would be correct. It is released when the
                        // thread exits.
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
                        eprintln!("[audio-prio] MMCSS 'Pro Audio' active on decode thread");
                    }
                    _ => {
                        // MMCSS can be unavailable (service disabled, or running in
                        // a container). A plain priority bump still helps.
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
                        eprintln!("[audio-prio] MMCSS unavailable; using above-normal priority");
                    }
                }
            }
        });
    }

    /// Raises the calling thread one step above normal. Used for the threads that
    /// *feed* the decoder (network read-ahead, playback worker) so they stay ahead
    /// of the audio callback without competing with it.
    pub fn raise_current_thread() {
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
        }
    }

    /// Drops the calling thread below normal. Used by the background fetch pool so
    /// startup enrichment work yields to the UI and to playback.
    pub fn lower_current_thread() {
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    pub fn register_audio_thread() {}
    pub fn raise_current_thread() {}
    pub fn lower_current_thread() {}
}

pub use imp::{lower_current_thread, raise_current_thread, register_audio_thread};
