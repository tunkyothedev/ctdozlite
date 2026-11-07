//! Embed `src/assets/app_icon.ico` into the Windows PE executable (`RT_ICON` + `RT_GROUP_ICON`)
//! by emitting a standard COFF/Win32 `.res` file in `OUT_DIR` without requiring external `rc.exe`.

fn main() {
    println!("cargo:rerun-if-changed=src/assets/app_icon.ico");
    println!("cargo:rerun-if-changed=src/assets/app_icon.png");

    #[cfg(target_os = "windows")]
    {
        if let Err(e) = embed_windows_icon() {
            println!("cargo:warning=Could not embed Windows icon resource: {e}");
        }
    }
}

#[cfg(target_os = "windows")]
fn embed_windows_icon() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;

    let ico = std::fs::read("src/assets/app_icon.ico")?;
    if ico.len() < 6 || u16::from_le_bytes([ico[0], ico[1]]) != 0 || u16::from_le_bytes([ico[2], ico[3]]) != 1 {
        return Err("Invalid ICO header".into());
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    if ico.len() < 6 + count * 16 {
        return Err("Truncated ICO directory".into());
    }

    let mut res = Vec::with_capacity(ico.len() + 512);

    // 1. 32-byte null resource header required at the start of every 32-bit .res file
    res.extend_from_slice(&0u32.to_le_bytes()); // DataSize = 0
    res.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize = 32
    res.extend_from_slice(&[0xFF, 0xFF, 0x00, 0x00]); // Type = 0
    res.extend_from_slice(&[0xFF, 0xFF, 0x00, 0x00]); // Name = 0
    res.extend_from_slice(&[0u8; 16]);

    // 2. Emit each RT_ICON (type = 3) entry with ordinal 1..=count
    let mut grp_dir = Vec::with_capacity(6 + count * 14);
    grp_dir.extend_from_slice(&ico[0..6]); // idReserved, idType, idCount

    for i in 0..count {
        let entry_off = 6 + i * 16;
        let entry = &ico[entry_off..entry_off + 16];
        let bytes_in_res = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        let img_off = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
        if img_off + bytes_in_res > ico.len() {
            return Err("ICO image slice out of bounds".into());
        }
        let icon_id = (i + 1) as u16;

        // Append GRPICONDIRENTRY (12 bytes from ICONDIRENTRY + u16 icon_id)
        grp_dir.extend_from_slice(&entry[0..12]);
        grp_dir.extend_from_slice(&icon_id.to_le_bytes());

        // Write RT_ICON resource header (32 bytes)
        write_res_header(&mut res, bytes_in_res as u32, 3, icon_id, 0x1010);
        res.extend_from_slice(&ico[img_off..img_off + bytes_in_res]);
        align4(&mut res);
    }

    // 3. Emit RT_GROUP_ICON (type = 14) with ordinal 1 ("MAINICON")
    write_res_header(&mut res, grp_dir.len() as u32, 14, 1, 0x1030);
    res.extend_from_slice(&grp_dir);
    align4(&mut res);

    // 4. Emit RT_VERSION (type = 16, id = 1) with VS_VERSIONINFO + StringFileInfo (#58)
    let pkg_ver = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let ver_blob = build_vs_version_info(&pkg_ver);
    write_res_header(&mut res, ver_blob.len() as u32, 16, 1, 0x0030);
    res.extend_from_slice(&ver_blob);
    align4(&mut res);

    // 5. Emit RT_MANIFEST (type = 24, id = 1) with asInvoker + Win10/11 compatibility (#58)
    let manifest = build_windows_manifest(&pkg_ver);
    write_res_header(&mut res, manifest.len() as u32, 24, 1, 0x1030);
    res.extend_from_slice(manifest.as_bytes());
    align4(&mut res);

    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let res_path = out_dir.join("app_icon.res");
    std::fs::write(&res_path, &res)?;

    // GNU ld cannot read a raw .res; windres (MinGW binutils) converts it to a COFF object.
    let link_path = if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
        let obj_path = out_dir.join("app_icon.o");
        let ok = std::process::Command::new("windres")
            .args(["-J", "res", "-O", "coff", "-i"])
            .arg(&res_path)
            .arg("-o")
            .arg(&obj_path)
            .status()?
            .success();
        if !ok {
            return Err("windres failed".into());
        }
        obj_path
    } else {
        res_path
    };

    println!("cargo:rustc-link-arg-bin=ctdozlite={}", link_path.display());
    Ok(())
}

#[cfg(target_os = "windows")]
fn write_res_header(buf: &mut Vec<u8>, data_size: u32, res_type: u16, res_name: u16, mem_flags: u16) {
    buf.extend_from_slice(&data_size.to_le_bytes());
    buf.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize = 32
    buf.extend_from_slice(&0xFFFFu16.to_le_bytes());
    buf.extend_from_slice(&res_type.to_le_bytes());
    buf.extend_from_slice(&0xFFFFu16.to_le_bytes());
    buf.extend_from_slice(&res_name.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    buf.extend_from_slice(&mem_flags.to_le_bytes());
    buf.extend_from_slice(&0x0409u16.to_le_bytes()); // LanguageId (en-US)
    buf.extend_from_slice(&0u32.to_le_bytes()); // Version
    buf.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
}

#[cfg(target_os = "windows")]
fn align4(buf: &mut Vec<u8>) {
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

#[cfg(target_os = "windows")]
fn push_utf16_z(buf: &mut Vec<u8>, s: &str) {
    for u in s.encode_utf16() {
        buf.extend_from_slice(&u.to_le_bytes());
    }
    buf.extend_from_slice(&0u16.to_le_bytes());
}

#[cfg(target_os = "windows")]
fn build_version_node(key: &str, w_type: u16, value: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // placeholder wLength
    let val_len = if w_type == 1 {
        (value.len() / 2) as u16
    } else {
        value.len() as u16
    };
    out.extend_from_slice(&val_len.to_le_bytes());
    out.extend_from_slice(&w_type.to_le_bytes());
    push_utf16_z(&mut out, key);
    align4(&mut out);
    if !value.is_empty() {
        out.extend_from_slice(value);
        align4(&mut out);
    }
    for (idx, child) in children.iter().enumerate() {
        out.extend_from_slice(child);
        if idx + 1 < children.len() {
            align4(&mut out);
        }
    }
    let total_len = out.len() as u16;
    out[0..2].copy_from_slice(&total_len.to_le_bytes());
    out
}

#[cfg(target_os = "windows")]
fn build_vs_version_info(pkg_ver: &str) -> Vec<u8> {
    let mut parts = pkg_ver
        .trim_start_matches(['v', 'V'])
        .split(['.', '-', '+'])
        .filter_map(|p| p.parse::<u16>().ok());
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    let build = 0u16;

    let file_ver_ms = ((major as u32) << 16) | (minor as u32);
    let file_ver_ls = ((patch as u32) << 16) | (build as u32);

    // VS_FIXEDFILEINFO (52 bytes)
    let mut fixed = Vec::with_capacity(52);
    fixed.extend_from_slice(&0xFEEF04BDu32.to_le_bytes()); // dwSignature
    fixed.extend_from_slice(&0x00010000u32.to_le_bytes()); // dwStrucVersion
    fixed.extend_from_slice(&file_ver_ms.to_le_bytes()); // dwFileVersionMS
    fixed.extend_from_slice(&file_ver_ls.to_le_bytes()); // dwFileVersionLS
    fixed.extend_from_slice(&file_ver_ms.to_le_bytes()); // dwProductVersionMS
    fixed.extend_from_slice(&file_ver_ls.to_le_bytes()); // dwProductVersionLS
    fixed.extend_from_slice(&0x0000003Fu32.to_le_bytes()); // dwFileFlagsMask
    fixed.extend_from_slice(&0x00000000u32.to_le_bytes()); // dwFileFlags
    fixed.extend_from_slice(&0x00040004u32.to_le_bytes()); // dwFileOS = VOS_NT_WINDOWS32
    fixed.extend_from_slice(&0x00000001u32.to_le_bytes()); // dwFileType = VFT_APP
    fixed.extend_from_slice(&0x00000000u32.to_le_bytes()); // dwFileSubtype
    fixed.extend_from_slice(&0x00000000u32.to_le_bytes()); // dwFileDateMS
    fixed.extend_from_slice(&0x00000000u32.to_le_bytes()); // dwFileDateLS

    let ver_quad = format!("{major}.{minor}.{patch}.{build}");
    let string_pairs = [
        ("CompanyName", "ctdoteam1110"),
        ("FileDescription", "ctdozlite Native Desktop Client"),
        ("FileVersion", ver_quad.as_str()),
        ("InternalName", "ctdozlite.exe"),
        ("LegalCopyright", "Copyright (c) 2026 ctdoteam1110 (MIT OR Apache-2.0)"),
        ("OriginalFilename", "ctdozlite.exe"),
        ("ProductName", "ctdozlite"),
        ("ProductVersion", pkg_ver),
    ];

    let mut string_nodes = Vec::with_capacity(string_pairs.len());
    for (k, v) in string_pairs {
        let mut val_utf16 = Vec::new();
        push_utf16_z(&mut val_utf16, v);
        string_nodes.push(build_version_node(k, 1, &val_utf16, &[]));
    }

    let string_table = build_version_node("040904B0", 1, &[], &string_nodes);
    let string_file_info = build_version_node("StringFileInfo", 1, &[], &[string_table]);

    let translation_val = [0x09, 0x04, 0xB0, 0x04]; // 0x0409 (en-US), 1200 (UTF-16)
    let var_node = build_version_node("Translation", 0, &translation_val, &[]);
    let var_file_info = build_version_node("VarFileInfo", 1, &[], &[var_node]);

    build_version_node(
        "VS_VERSION_INFO",
        0,
        &fixed,
        &[string_file_info, var_file_info],
    )
}

#[cfg(target_os = "windows")]
fn build_windows_manifest(pkg_ver: &str) -> String {
    let mut parts = pkg_ver
        .trim_start_matches(['v', 'V'])
        .split(['.', '-', '+'])
        .filter_map(|p| p.parse::<u16>().ok());
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="ctdoteam1110.ctdozlite" version="{major}.{minor}.{patch}.0" processorArchitecture="amd64"/>
  <description>ctdozlite Native Desktop Client</description>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}"/>
      <supportedOS Id="{{1f676c76-80e1-4239-95bb-83d0f6d0da78}}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <activeCodePage xmlns="http://schemas.microsoft.com/SMI/2019/WindowsSettings">UTF-8</activeCodePage>
    </windowsSettings>
  </application>
</assembly>"#
    )
}
