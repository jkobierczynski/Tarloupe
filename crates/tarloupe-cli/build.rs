fn main() {
    // WinFsp's DLL is delay-loaded, so binaries start without it and can
    // explain how to install it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winfsp::build::winfsp_link_delayload();
    }
}
