fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // WinFsp's DLL is delay-loaded, so binaries start without it and can
        // explain how to install it.
        winfsp::build::winfsp_link_delayload();
        // The icon, as resource 1. It needs the Windows resource compiler,
        // so cross builds from other systems go without.
        if cfg!(windows) {
            embed_resource::compile("assets/tarloupe.rc", embed_resource::NONE)
                .manifest_optional()
                .expect("cannot compile the icon resource");
        }
    }
}
