//! Protection against denormal floats on the audio thread.
//!
//! Denormals are numbers so close to zero (below ~1.2e-38) that x86 CPUs
//! process them in slow microcode, often 50-100x slower per operation.
//! Audio code produces them naturally: every filter, envelope or reverb
//! decaying toward silence passes through them. They can also appear from
//! optimizer-generated speculative code. The result is sudden CPU spikes,
//! heard as dropouts.
//!
//! The fix used throughout the industry (JUCE's `ScopedNoDenormals`, for
//! example) is to switch the CPU into flush-to-zero mode for the duration
//! of the audio callback. Denormals are around -750 dB, so replacing them
//! with zero is inaudible.

/// While alive, denormal results are flushed to zero (FTZ) and denormal
/// inputs are treated as zero (DAZ). The previous mode is restored on drop.
pub struct NoDenormals {
    #[cfg(target_arch = "x86_64")]
    previous: u32,
}

impl NoDenormals {
    #[cfg(target_arch = "x86_64")]
    #[inline]
    pub fn new() -> Self {
        const FTZ: u32 = 1 << 15;
        const DAZ: u32 = 1 << 6;
        let mut previous: u32 = 0;
        // SAFETY: stmxcsr/ldmxcsr only read and write the SSE control
        // register through a valid pointer to a local u32.
        unsafe {
            std::arch::asm!("stmxcsr [{}]", in(reg) &mut previous, options(nostack, preserves_flags));
            let mode = previous | FTZ | DAZ;
            std::arch::asm!("ldmxcsr [{}]", in(reg) &mode, options(nostack, preserves_flags, readonly));
        }
        Self { previous }
    }

    /// Other architectures: a no-op. (Recent ARM cores, such as Apple
    /// Silicon, handle denormals in hardware without the big penalty; an
    /// ARM version would set the FZ bit in the FPCR register.)
    #[cfg(not(target_arch = "x86_64"))]
    #[inline]
    pub fn new() -> Self {
        Self {}
    }
}

impl Drop for NoDenormals {
    #[inline]
    fn drop(&mut self) {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: as above.
        unsafe {
            std::arch::asm!("ldmxcsr [{}]", in(reg) &self.previous, options(nostack, preserves_flags, readonly));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flushes_inside_and_restores_after() {
        let tiny = std::hint::black_box(1e-30f32);
        let scale = std::hint::black_box(1e-10f32);
        assert!((tiny * scale).is_subnormal(), "normally the product is a denormal");
        {
            let _guard = NoDenormals::new();
            if cfg!(target_arch = "x86_64") {
                assert_eq!(tiny * std::hint::black_box(scale), 0.0, "flushed to zero");
            }
        }
        assert!((tiny * std::hint::black_box(scale)).is_subnormal(), "mode restored");
    }
}
