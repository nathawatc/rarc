fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    pkg_config::Config::new()
        .atleast_version("3.2.0")
        .probe("libarchive")
        .expect("libarchive >= 3.2.0 not found via pkg-config");
}
