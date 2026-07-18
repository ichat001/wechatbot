// rust/build.rs
fn main() {
    // 仅 Windows 平台需要嵌入版本信息
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() != "windows" {
        return;
    }

    let major = std::env::var("APP_VERSION_MAJOR").unwrap_or_else(|_| "0".into());
    let minor = std::env::var("APP_VERSION_MINOR").unwrap_or_else(|_| "0".into());
    let build = std::env::var("APP_VERSION_BUILD").unwrap_or_else(|_| "0".into());
    let revision = std::env::var("APP_VERSION_REVISION").unwrap_or_else(|_| "0".into());

    let version_string = format!("{}.{}.{}.{}", major, minor, build, revision);

    let mut res = winres::WindowsResource::new();
    res.set("FileVersion", &version_string);
    res.set("ProductVersion", &version_string);
    res.set("ProductName", "n8n_bot");
    res.set("LegalCopyright", "Copyright © YourName");

    // 数字版本（四个 u16 数值）
    res.set_version_info(
        winres::VersionInfo::FILEVERSION,
        (major.parse().unwrap_or(0),
         minor.parse().unwrap_or(0),
         build.parse().unwrap_or(0),
         revision.parse().unwrap_or(0)),
    );
    res.set_version_info(
        winres::VersionInfo::PRODUCTVERSION,
        (major.parse().unwrap_or(0),
         minor.parse().unwrap_or(0),
         build.parse().unwrap_or(0),
         revision.parse().unwrap_or(0)),
    );

    if let Err(e) = res.compile() {
        eprintln!("winres 编译失败: {}", e);
        std::process::exit(1);
    }
}
