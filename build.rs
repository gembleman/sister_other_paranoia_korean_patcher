fn main() {
    println!("cargo:rerun-if-changed=assets/botan-face-icon.ico");
    println!("cargo:rerun-if-changed=payloads/");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut resource = winres::WindowsResource::new();
        resource.set_icon("assets/botan-face-icon.ico");
        resource
            .compile()
            .expect("failed to embed the application icon");
    }
}
