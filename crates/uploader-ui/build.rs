fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app-dev.ico");
    println!("cargo:rerun-if-changed=assets/app-dev.rc");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let resource = if std::env::var("DEBUG").as_deref() == Ok("true") {
            "assets/app-dev.rc"
        } else {
            "assets/app.rc"
        };
        embed_resource::compile(resource, embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
}
