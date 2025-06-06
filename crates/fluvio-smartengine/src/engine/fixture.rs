use std::path::{PathBuf, Path};

#[cfg(test)]
pub(crate) fn read_wasm_module(module_name: &str) -> (String, Vec<u8>) {
    let spu_dir = std::env::var("CARGO_MANIFEST_DIR").expect("target");
    let wasm_path = PathBuf::from(spu_dir)
        .parent()
        .expect("parent")
        .parent()
        .expect("fluvio")
        .join(format!(
            "smartmodule/examples/target/wasm32-wasip1/release-lto/{module_name}.wasm"
        ));
    let name = module_name.to_string();
    (name, read_module_from_path(wasm_path))
}

#[cfg(test)]
pub(crate) fn read_module_from_path(filter_path: impl AsRef<Path>) -> Vec<u8> {
    let path = filter_path.as_ref();
    std::fs::read(path).unwrap_or_else(|_| panic!("Unable to read file {}", path.display()))
}
