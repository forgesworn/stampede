//! Desktop launcher (Android enters through `stampede::android_main`).

#[cfg(not(target_os = "android"))]
fn main() {
    std::process::exit(stampede::run_desktop());
}

#[cfg(target_os = "android")]
fn main() {}
