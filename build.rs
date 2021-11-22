extern crate cbindgen;

use std::env;

fn main() {
    stderrlog::new()
    .verbosity(4)
    // .timestamp(stderrlog::Timestamp::Millisecond)
    .modules(["cbindgen", "ufo_c", "ufo_core"])
    .init()
    .unwrap();

    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let mut config: cbindgen::Config = Default::default();
    config.language = cbindgen::Language::C;

    cbindgen::Builder::new()
        .with_config(config)
        .with_crate(&crate_dir)

        .with_parse_deps(true)
        .with_parse_include(&["ufo_core"])
        .with_parse_extra_bindings(&["ufo_core"])

        .with_pragma_once(true)
        .with_autogen_warning("/* Generated by cbindgen, do not edit by hand */")
        .with_trailer("#define strideOf(_type) ( (uint32_t) (uint64_t) ( ((_type*) 0) + 1) )\n")

        .generate()
        .unwrap()
        .write_to_file("target/ufo_c.h");
}
