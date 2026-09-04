// Embed the Windows application icon into the .exe at build time.
//
// On Windows, winresource compiles an `app.rc` that assigns the .ico to the binary,
// so Explorer / the taskbar / the installer all show the MouseShare logo instead of
// the generic executable glyph. On macOS and Linux this is a no-op (the icon is
// provided by the .app bundle's AppIcon.icns and by the runtime viewport icon).

#[cfg(target_os = "windows")]
fn main() {
    let mut res = winresource::WindowsResource::new();
    res.set_icon("resources/AppIcon.ico");
    res.compile().expect("failed to embed Windows icon resource");
}

#[cfg(not(target_os = "windows"))]
fn main() {}
