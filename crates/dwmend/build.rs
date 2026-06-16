// Embed the Win32 manifest (DPI awareness, long-path support, supportedOS),
// the application icon (assets/icon.ico — 4-tile dwindle / BSP layout) and
// a VS_VERSION_INFO resource into dwmend.exe so the OS picks them up at load
// time and Explorer's File Properties dialog shows version / product /
// company metadata. The .rc is regenerated on every build from Cargo
// package metadata so the embedded strings stay in sync with `Cargo.toml`.

fn main() {
    // Re-run whenever inputs that feed the .rc change.
    println!("cargo:rerun-if-changed=manifest.xml");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_AUTHORS");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_DESCRIPTION");

    // Resolve the icon path relative to this crate's manifest dir so the
    // .rc reference is unambiguous regardless of where cargo is invoked
    // from. Forward slashes work in rc.exe path strings and avoid the
    // C-escape gymnastics that backslashes would require.
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo");
    let icon_path = std::path::Path::new(&manifest_dir)
        .join("..")
        .join("..")
        .join("assets")
        .join("icon.ico");
    let icon_str = icon_path.to_string_lossy().replace('\\', "/");
    println!("cargo:rerun-if-changed={}", icon_path.display());

    // Parse `CARGO_PKG_VERSION` (e.g. "0.1.0") into the comma/dotted forms
    // VS_VERSION_INFO requires. The 4th component (build) is always 0.
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut parts = version.split('.').map(|s| s.parse::<u32>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    let comma_ver = format!("{major},{minor},{patch},0");
    let dotted_ver = format!("{major}.{minor}.{patch}.0");

    // `CARGO_PKG_AUTHORS` is colon-separated when multiple are declared.
    let authors = std::env::var("CARGO_PKG_AUTHORS")
        .unwrap_or_else(|_| "DWMend contributors".into())
        .replace(':', ", ");
    let description = std::env::var("CARGO_PKG_DESCRIPTION").unwrap_or_else(|_| "DWMend".into());

    // Update yearly. Year-bumps are fine; the legal copyright line just
    // needs to reflect the most recent significant release.
    let copyright_year = 2026;

    // 040904B0 = US English (0x0409) + Unicode (0x04B0). `BLOCK` / `VALUE`
    // are the standard VS_VERSION_INFO keywords parsed by rc.exe.
    //
    // `1 ICON` registers the multi-resolution dwindle/BSP icon as
    // RT_GROUP_ICON #1 (plus auto-numbered RT_ICON entries per sub-image).
    // Explorer picks the lowest-numbered RT_GROUP_ICON for the file's
    // shell icon, and tray.rs loads it back via `LoadIconW(.., 1)`.
    let rc_text = format!(
        "#include <winuser.h>\r\n\
         1 RT_MANIFEST \"manifest.xml\"\r\n\
         1 ICON \"{icon_str}\"\r\n\
         \r\n\
         1 VERSIONINFO\r\n\
         FILEVERSION {comma_ver}\r\n\
         PRODUCTVERSION {comma_ver}\r\n\
         FILEOS 0x40004L\r\n\
         FILETYPE 0x1L\r\n\
         {{\r\n\
         \tBLOCK \"StringFileInfo\"\r\n\
         \t{{\r\n\
         \t\tBLOCK \"040904B0\"\r\n\
         \t\t{{\r\n\
         \t\t\tVALUE \"CompanyName\",      \"{authors}\"\r\n\
         \t\t\tVALUE \"FileDescription\",  \"{description}\"\r\n\
         \t\t\tVALUE \"FileVersion\",      \"{dotted_ver}\"\r\n\
         \t\t\tVALUE \"InternalName\",     \"dwmend\"\r\n\
         \t\t\tVALUE \"LegalCopyright\",   \"Copyright (C) {copyright_year} {authors}\"\r\n\
         \t\t\tVALUE \"OriginalFilename\", \"dwmend.exe\"\r\n\
         \t\t\tVALUE \"ProductName\",      \"DWMend\"\r\n\
         \t\t\tVALUE \"ProductVersion\",   \"{dotted_ver}\"\r\n\
         \t\t}}\r\n\
         \t}}\r\n\
         \tBLOCK \"VarFileInfo\"\r\n\
         \t{{\r\n\
         \t\tVALUE \"Translation\", 0x0409, 0x04B0\r\n\
         \t}}\r\n\
         }}\r\n",
    );

    std::fs::write("dwmend.rc", rc_text).expect("write dwmend.rc");

    embed_resource::compile("dwmend.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
