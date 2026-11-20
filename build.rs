//! Embeds the Snippy icon and version info into the Windows executable.
fn main() {
    println!("cargo:rerun-if-changed=assets/snippy.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/snippy.ico")
            .set("FileDescription", "Snippy — screen snipping & recording")
            .set("ProductName", "Snippy");
        if let Err(e) = res.compile() {
            // Don't fail the build over an icon (e.g. no resource compiler available).
            println!("cargo:warning=couldn't embed the app icon: {e}");
        }
    }
}
