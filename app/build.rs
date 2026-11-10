fn main() {
    // Icon resource ID 1 is what gpui loads for the window and taskbar on Windows.
    // Gated on the host, because winresource is only a dependency there (it needs rc.exe).
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=../assets/icon.ico");
        winresource::WindowsResource::new().set_icon("../assets/icon.ico").compile().expect("embed icon");
    }
}
