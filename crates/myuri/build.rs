use std::{env, fs, path::PathBuf};

fn main() {
    for file in ["myuri.rc", "myuri.ico", "myuri-paused.ico", "myuri.manifest.in"] {
        println!("cargo:rerun-if-changed=assets/{file}");
    }

    let [major, minor, patch] = [
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        env!("CARGO_PKG_VERSION_PATCH"),
    ]
    .map(|part| part.parse::<u16>().expect("Windows version parts must fit in a u16"));
    let full_version = env!("CARGO_PKG_VERSION");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    fs::write(
        out.join("myuri-version.h"),
        format!("#define MYURI_VERSION_NUMBER {major},{minor},{patch},0\n#define MYURI_VERSION_STRING \"{full_version}\"\n"),
    )
    .expect("write Windows version header");
    fs::write(
        out.join("myuri.manifest"),
        include_str!("assets/myuri.manifest.in").replace("@VERSION@", &format!("{major}.{minor}.{patch}.0")),
    )
    .expect("write Windows application manifest");

    embed_resource::compile("assets/myuri.rc", embed_resource::ParamsIncludeDirs([out]))
        .manifest_required()
        .expect("compile Windows executable resources");
}
