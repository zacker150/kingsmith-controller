//! One-line logging without a logging framework: stderr on desktop, logcat
//! (tag `KSCTRL`) on Android.

pub fn line(msg: &str) {
    #[cfg(target_os = "android")]
    android::write(msg);
    #[cfg(not(target_os = "android"))]
    eprintln!("[kingsmith] {msg}");
}

#[cfg(target_os = "android")]
mod android {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    pub fn write(msg: &str) {
        const INFO: c_int = 4;
        let tag = c"KSCTRL";
        let text = CString::new(msg.replace('\0', "\\0")).unwrap_or_default();
        // SAFETY: both pointers are valid NUL-terminated strings for the call.
        unsafe {
            __android_log_write(INFO, tag.as_ptr(), text.as_ptr());
        }
    }
}
