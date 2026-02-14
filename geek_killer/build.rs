use rust_core_lib::meta::STAR_TAP_BRAND;

fn main() {
    let is_windows = std::env::var("CARGO_CFG_TARGET_OS")
        .map(|v| v == "windows")
        .unwrap_or(false);
    if is_windows {
        let is_release = std::env::var("PROFILE")
            .map(|v| v == "release")
            .unwrap_or(false);
        let mut res = winres::WindowsResource::new();
        res.set_icon("../进程图标.ico");
        res.set("CompanyName", STAR_TAP_BRAND.lab_name);
        res.set(
            "FileDescription",
            "Geek Killer Pro - 高级进程 management 工具",
        );
        res.set("LegalCopyright", &STAR_TAP_BRAND.legal_copyright());
        res.set("ProductName", "Geek Killer Pro");
        res.set("InternalName", "geek_killer.exe");
        if is_release {
            res.set_manifest_file("app.manifest");
        }
        res.compile().unwrap();
    }
}
