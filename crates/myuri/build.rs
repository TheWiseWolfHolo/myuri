fn main() {
    for file in ["myuri.rc", "myuri.ico", "myuri-paused.ico"] {
        println!("cargo:rerun-if-changed=assets/{file}");
    }
    // Icon 1 is what Windows and GPUI show for the program and its windows.
    embed_resource::compile("assets/myuri.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("the icon resource compiles");
}
