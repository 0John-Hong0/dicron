fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    if target_os != "windows" {
        return;
    }

    let mut windows_resource = winresource::WindowsResource::new();
    windows_resource.set_icon("assets/icon.ico");

    windows_resource
        .compile()
        .expect("failed to embed Windows icon");
}
