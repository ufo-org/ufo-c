extern crate cbindgen;

use std::env;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let mut config: cbindgen::Config = Default::default();
    config.language = cbindgen::Language::C;

    cbindgen::Builder::new()
        .with_config(config)
        .with_crate(&crate_dir)
        .with_pragma_once(true)
        .with_autogen_warning("/* Generated by cbindgen, do not edit by hand */")
        .with_trailer("#define strideOf(_type) ( (uint32_t) (uint64_t) ( ((_type*) 0) + 1) )\n")
        .generate()
        .unwrap()
        .write_to_file("target/ufos_c.h");
}