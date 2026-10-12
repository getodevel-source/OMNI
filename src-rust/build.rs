fn main() {
    println!("cargo:rerun-if-changed=omni.rc");
    println!("cargo:rerun-if-changed=assets/omni.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("omni.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("No se pudo incrustar el icono de OMNI en el ejecutable de Windows");
    }
}
