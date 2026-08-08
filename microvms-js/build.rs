//! The one build-time requirement napi-rs has.
//!
//! `napi_build::setup()` emits the link configuration the addon needs. It is not a
//! node-gyp step and it does not want Node headers — since napi-build 2 / napi-sys 3,
//! Node-API symbols are resolved at *runtime* through `libloading` on every platform,
//! because the weak-symbol linker tricks the older versions used were found unsound.
//! Omitting this call does not warn; it produces undefined-symbol link failures, which is
//! why it is here rather than assumed.

fn main() {
    napi_build::setup();
}
