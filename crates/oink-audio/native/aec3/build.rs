fn main() {
    let windows = std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows";
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let apple = matches!(target_os.as_str(), "macos" | "ios");
    let android = target_os == "android";
    let arm = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap() == "aarch64";
    let mut cpp = cc::Build::new();
    cpp.cpp(true).std("c++17").opt_level(3).warnings(false)
        .include("vendor/webrtc").include("vendor/abseil")
        .define("NDEBUG", None).define("WEBRTC_APM_DEBUG_DUMP", "0")
        .define("RTC_DISABLE_LOGGING", None).define("RTC_DISABLE_METRICS", None)
        .define("WEBRTC_LIBRARY_IMPL", None)
        // Private namespace: no collisions with a separately linked WebRTC.
        .define("webrtc", "oink_webrtc").define("rtc", "oink_rtc")
        .define("absl", "oink_absl");
    if windows {
        // The Windows workspace uses the static MSVC CRT for native owners
        // (oink-winsvc is the reference). Keep every AEC object on /MT so a
        // test or agent executable cannot combine /MD and /MT objects.
        cpp.static_crt(true);
    }
    for symbol in ["WebRtcSpl_AnalysisQMF", "WebRtcSpl_SynthesisQMF", "rtc_FatalMessage"] {
        cpp.define(symbol, format!("oink_{symbol}").as_str());
    }
    if windows {
        cpp.flag("/EHsc")
            .define("WEBRTC_WIN", None)
            .define("NOMINMAX", None)
            .define("_USE_MATH_DEFINES", None)
            .define("_WINSOCKAPI_", None);
    } else {
        cpp.define("WEBRTC_POSIX", None);
        if apple {
            cpp.define("WEBRTC_MAC", None);
        } else if android {
            // The vendored WebRTC thread shim uses WEBRTC_LINUX for the
            // Android prctl include, while WEBRTC_ANDROID selects gettid().
            cpp.define("WEBRTC_ANDROID", None).define("WEBRTC_LINUX", None);
        }
    }
    if !arm {
        let mut avx = cpp.clone();
        avx.flag(if windows { "/arch:AVX2" } else { "-mavx2" });
        if !windows {
            avx.flag("-mfma");
        }
        for source in include_str!("avx2-sources.txt").lines() {
            avx.file(format!("vendor/webrtc/{source}"));
        }
        avx.compile("oink_aec3_avx2");
    }
    if arm {
        cpp.define("WEBRTC_ARCH_ARM64", None).define("WEBRTC_HAS_NEON", None);
        cpp.file("vendor/webrtc/common_audio/resampler/sinc_resampler_neon.cc")
            .file("vendor/webrtc/common_audio/third_party/ooura/fft_size_128/ooura_fft_neon.cc");
    } else {
        cpp.file("vendor/webrtc/common_audio/resampler/sinc_resampler_sse.cc")
            .file("vendor/webrtc/common_audio/third_party/ooura/fft_size_128/ooura_fft_sse2.cc");
    }
    let mut c = cc::Build::new();
    c.include("vendor/webrtc")
        .define("NDEBUG", None)
        .warnings(false)
        .opt_level(3);
    if windows {
        c.static_crt(true);
    }
    for symbol in ["WebRtcSpl_AnalysisQMF", "WebRtcSpl_SynthesisQMF"] {
        c.define(symbol, format!("oink_{symbol}").as_str());
    }
    for source in include_str!("sources.txt").lines() {
        let path = format!("vendor/webrtc/{source}");
        if source.ends_with(".c") {
            c.file(path);
        } else {
            cpp.file(path);
        }
    }
    cpp.file("bridge.cc").compile("oink_aec3");
    c.compile("oink_aec3_c");
    println!("cargo:rerun-if-changed=vendor");
    println!("cargo:rerun-if-changed=bridge.cc");
    println!("cargo:rerun-if-changed=sources.txt");
}
