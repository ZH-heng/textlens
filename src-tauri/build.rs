fn main() {
    println!("cargo:rerun-if-changed=../apps/macos/native/selection_bridge.h");
    println!("cargo:rerun-if-changed=../apps/macos/native/app_profiles.h");
    println!("cargo:rerun-if-changed=../apps/macos/native/selection_policy.h");
    println!("cargo:rerun-if-changed=../apps/macos/native/selection_bridge.mm");
    println!("cargo:rerun-if-changed=../apps/macos/native/LICENSE.selection-hook");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .cpp(true)
            .file("../apps/macos/native/selection_bridge.mm")
            .include("../apps/macos/native")
            .flag_if_supported("-std=c++17")
            .flag_if_supported("-fobjc-arc")
            .warnings(true)
            .flag_if_supported("-Werror")
            .compile("textlens_selection_bridge");

        for framework in [
            "AppKit",
            "ApplicationServices",
            "Carbon",
            "CoreGraphics",
            "Foundation",
        ] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
    }

    tauri_build::build()
}
