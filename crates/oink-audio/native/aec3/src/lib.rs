//! Fixed-buffer single-owner AEC3 binding. Creation/destruction belong to control threads.
use std::ffi::c_void;
unsafe extern "C" {
    fn oink_aec3_create() -> *mut c_void;
    fn oink_aec3_destroy(p: *mut c_void);
    fn oink_aec3_process(
        p: *mut c_void,
        left: *const f32,
        right: *const f32,
        mic: *const f32,
        output: *mut f32,
        linear: *mut f32,
        level_change: i32,
    );
    fn oink_aec3_linear_powers(p: *mut c_void, capture: *mut f64, linear: *mut f64);
    fn oink_aec3_metrics(p: *mut c_void, erl: *mut f64, erle: *mut f64, delay: *mut i32);
}
#[derive(Debug)]
pub struct Engine(*mut c_void);
// SAFETY: the engine has a unique owner; all calls require mutable access.
unsafe impl Send for Engine {}
impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}
impl Engine {
    pub fn new() -> Self {
        Self::try_new().expect("AEC3 initialization")
    }
    pub fn try_new() -> Result<Self, &'static str> {
        // SAFETY: constructor has no arguments and catches native exceptions.
        let ptr = unsafe { oink_aec3_create() };
        if ptr.is_null() {
            Err("AEC3 allocation failed")
        } else {
            Ok(Self(ptr))
        }
    }
    pub fn process(
        &mut self,
        reference: &[[f32; 480]; 2],
        mic: &[f32; 480],
        output: &mut [f32; 480],
        linear: &mut [f32; 160],
        level_change: bool,
    ) {
        // SAFETY: all arrays have the exact ABI lengths and remain valid for this call.
        unsafe {
            oink_aec3_process(
                self.0,
                reference[0].as_ptr(),
                reference[1].as_ptr(),
                mic.as_ptr(),
                output.as_mut_ptr(),
                linear.as_mut_ptr(),
                i32::from(level_change),
            )
        }
    }
    pub fn linear_powers(&mut self) -> (f64, f64) {
        let (mut capture, mut linear) = (0.0, 0.0);
        // SAFETY: uniquely owned engine and writable scalar output pointers.
        unsafe { oink_aec3_linear_powers(self.0, &mut capture, &mut linear) };
        (capture, linear)
    }
    pub fn metrics(&mut self) -> (f64, f64, i32) {
        let (mut erl, mut erle, mut delay) = (0.0, 0.0, 0);
        // SAFETY: uniquely owned engine and writable scalar output pointers.
        unsafe { oink_aec3_metrics(self.0, &mut erl, &mut erle, &mut delay) };
        (erl, erle, delay)
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: this unique owner destroys the successfully constructed handle once.
        unsafe { oink_aec3_destroy(self.0) }
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn linear_power_is_aligned_with_the_capture_low_band() {
        let mut engine = super::Engine::new();
        let mut output = [0.0; 480];
        let mut linear = [0.0; 160];
        let mut seed = 93_u32;
        let mut difference = 0.0;
        let mut total = 0.0;
        for _ in 0..100 {
            let mic = std::array::from_fn(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                ((seed >> 8) as f32 / 8388608.0 - 1.0) * 0.1
            });
            engine.process(&[[0.0; 480]; 2], &mic, &mut output, &mut linear, false);
            let (capture, error) = engine.linear_powers();
            difference += (capture - error).abs();
            total += capture;
        }
        assert!(
            difference / total < 1e-5,
            "unaligned linear power: {}",
            difference / total
        );
    }
    #[test]
    fn silence_is_finite() {
        let mut engine = super::Engine::new();
        let mut out = [0.0; 480];
        let mut linear = [0.0; 160];
        for _ in 0..1000 {
            engine.process(&[[0.0; 480]; 2], &[0.0; 480], &mut out, &mut linear, false);
            assert!(out.iter().all(|v| v.is_finite()));
        }
    }
}
