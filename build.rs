fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=assets/icon.ico");

    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.set_language(0x0804);
    res.compile()
        .expect("winresource 编译失败：确认 assets/icon.ico 存在，且使用 MSVC 工具链");
}
