extern crate cbindgen;

use std::env;
use std::path::PathBuf;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_path = PathBuf::from(&crate_dir).join("nedb.h");

    cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_language(cbindgen::Language::C)
        .with_cpp_compat(true)
        .with_include_guard("NEDB_FFI_H")
        .with_documentation(true)
        .generate()
        .expect("Unable to generate nedb.h via cbindgen")
        .write_to_file(out_path);
}
