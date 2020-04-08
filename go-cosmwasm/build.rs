use std::env;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    cbindgen::generate(crate_dir)
        .expect("Unable to generate bindings")
        .write_to_file("./api/bindings.h");

    println!("cargo:rustc-link-search=native=./lib");
    println!("cargo:rustc-link-lib=static=Enclave_u");
    let is_sim = env::var("SGX_MODE").unwrap_or_else(|_| "HW".to_string());
    match is_sim.as_ref() {
        "SW" => println!("cargo:rustc-link-lib=dylib=sgx_urts_sim"),
        "HW" => println!("cargo:rustc-link-lib=dylib=sgx_urts"),
        // Treat undefined as HW
        _ => println!("cargo:rustc-link-lib=dylib=sgx_urts"),
    }
}
