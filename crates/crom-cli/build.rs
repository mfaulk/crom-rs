fn main() {
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".into());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=PROFILE_INFO={profile}");
    println!("cargo:rustc-env=TARGET_INFO={target}");
}
