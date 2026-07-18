fn main() {
    // 仅 Windows 平台需要嵌入版本信息
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() != "windows" {
        return;
    }

    // 从环境变量读取，默认值 0
    let major: u64 = std::env::var("APP_VERSION_MAJOR")
        .unwrap_or_default()
        .parse()
        .unwrap_or(0);
    let minor: u64 = std::env::var("APP_VERSION_MINOR")
        .unwrap_or_default()
        .parse()
        .unwrap_or(0);
    let build: u64 = std::env::var("APP_VERSION_BUILD")
        .unwrap_or_default()
        .parse()
        .unwrap_or(0);
    let revision: u64 = std::env::var("APP_VERSION_REVISION")
        .unwrap_or_default()
        .parse()
        .unwrap_or(0);

    // 组合成 u64：每部分 16 位
    let version_num = (major << 48) | (minor << 32) | (build << 16) | revision;

    // 字符串形式用于 FileVersion/ProductVersion 属性
    let version_string = format!("{}.{}.{}.{}", major, minor, build, revision);

    let mut res = winres::WindowsResource::new();
    res.set("FileVersion", &version_string);
    res.set("ProductVersion", &version_string);
    res.set("ProductName", "n8n_bot");
    res.set("LegalCopyright", "Copyright © YourName");

    // 设置数字版本号（必须是 u64）
    res.set_version_info(winres::VersionInfo::FILEVERSION, version_num);
    res.set_version_info(winres::VersionInfo::PRODUCTVERSION, version_num);

    if let Err(e) = res.compile() {
        eprintln!("winres 编译失败: {}", e);
        std::process::exit(1);
    }
}
