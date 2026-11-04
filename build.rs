use std::env;
use std::fs;
use std::path::Path;

/// `(name, include_bytes!(path))` entries for the `<name>.json.zst` files
/// of `dir`, sorted by name.
fn table(dir: &Path, var: &str) -> String {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".json.zst").map(str::to_string)
        })
        .collect();
    names.sort();
    let mut code = format!("pub static {var}: &[(&str, &[u8])] = &[\n");
    for name in &names {
        let path = dir.join(format!("{name}.json.zst"));
        code.push_str(&format!(
            "    ({name:?}, include_bytes!({:?})),\n",
            path.display().to_string()
        ));
    }
    code.push_str("];\n");
    code
}

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let models = Path::new(&manifest_dir).join("models");
    println!("cargo:rerun-if-changed=models");

    fs::write(
        Path::new(&out_dir).join("models.rs"),
        table(&models.join("services"), "SERVICES"),
    )
    .unwrap();
    let mut docs = table(&models.join("examples"), "EXAMPLES");
    docs.push_str(&table(&models.join("help"), "HELP"));
    fs::write(Path::new(&out_dir).join("docs.rs"), docs).unwrap();
}
