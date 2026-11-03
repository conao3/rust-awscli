use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let services_dir = Path::new(&manifest_dir).join("models/services");
    println!("cargo:rerun-if-changed=models");

    let mut names: Vec<String> = fs::read_dir(&services_dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".json.zst").map(str::to_string)
        })
        .collect();
    names.sort();

    let mut code = String::from("pub static SERVICES: &[(&str, &[u8])] = &[\n");
    for name in &names {
        let path = services_dir.join(format!("{name}.json.zst"));
        code.push_str(&format!(
            "    ({name:?}, include_bytes!({:?})),\n",
            path.display().to_string()
        ));
    }
    code.push_str("];\n");
    fs::write(Path::new(&out_dir).join("models.rs"), code).unwrap();
}
