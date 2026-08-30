fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/apertureneo_turbo.ico");
        res.set("ProductName", "Aperture Neo Turbo");
        res.set("FileDescription", "Aperture Neo Turbo - High-performance GPU image viewer");
        res.set("FileVersion", "1.0.8");
        res.set("ProductVersion", "1.0.8");
        res.set("LegalCopyright", "DuJunxi1993");
        if let Err(e) = res.compile() {
            println!("cargo:warning=winresource embed failed: {e}");
        }
    }
}
