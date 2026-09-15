use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .expect("catalog directory")
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            files(root, &path, out);
        } else if path.extension().is_some_and(|ext| ext == "toml") {
            let id = path
                .strip_prefix(root)
                .unwrap()
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");
            out.push((id, path));
        }
    }
}
fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let mut generated = String::new();
    for (dir, constant) in [("vehicles", "VEHICLE_FILES"), ("ecus", "ECU_FILES")] {
        println!("cargo:rerun-if-changed={dir}");
        let mut entries = Vec::new();
        files(&root.join(dir), &root.join(dir), &mut entries);
        generated.push_str(&format!("pub static {constant}: &[(&str, &str)] = &[\n"));
        for (id, path) in &entries {
            generated.push_str(&format!("({id:?}, include_str!({path:?})),\n"));
        }
        generated.push_str("];\n");
        if dir == "ecus" {
            generated.push_str("pub static ECU_INHERITANCE: &[(&str, Option<&str>)] = &[\n");
            let mut variants = Vec::new();
            for (id, path) in &entries {
                let value: toml::Value = fs::read_to_string(path)
                    .unwrap()
                    .parse()
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                let parent = value
                    .get("extends")
                    .map(|value| value.as_str().expect("extends must be a definition ID"));
                if value
                    .get("match")
                    .and_then(|v| v.as_array())
                    .is_some_and(|matches| !matches.is_empty())
                {
                    variants.push(id);
                }
                generated.push_str(&format!("({id:?}, {parent:?}),\n"));
            }
            generated.push_str("];\n");
            generated.push_str(&format!(
                "pub static ECU_VARIANTS: &[&str] = &{variants:?};\n"
            ));
        }
        if dir == "vehicles" {
            generated.push_str("pub static INDEX: &[VehicleIndex] = &[\n");
            for (id, path) in entries {
                let text = fs::read_to_string(&path).unwrap();
                let value: toml::Value = text
                    .parse()
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                let string = |key: &str| {
                    value[key]
                        .as_str()
                        .unwrap_or_else(|| panic!("{}: {key} must be a string", path.display()))
                };
                let years = value
                    .get("years")
                    .and_then(|value| value.as_array())
                    .unwrap_or_else(|| {
                        panic!("{}: years must be a list of named ranges", path.display())
                    });
                let mut ranges = String::new();
                for (index, range) in years.iter().enumerate() {
                    let table = range.as_table().unwrap_or_else(|| {
                        panic!(
                            "{}: years[{index}] must have named from and to fields",
                            path.display()
                        )
                    });
                    let endpoint = |key: &str| {
                        table
                            .get(key)
                            .and_then(|value| value.as_integer())
                            .and_then(|year| u16::try_from(year).ok())
                            .unwrap_or_else(|| {
                                panic!("{}: years[{index}].{key} must be a year", path.display())
                            })
                    };
                    let from = endpoint("from");
                    let to = endpoint("to");
                    ranges.push_str(&format!("YearRange {{ from: {from}, to: {to} }},"));
                }
                let vins: Vec<_> = value
                    .get("vin")
                    .and_then(|v| v.as_array())
                    .map(|v| v.iter().map(|v| v.as_str().expect("VIN prefix")).collect())
                    .unwrap_or_default();
                generated.push_str(&format!("VehicleIndex {{ id: {id:?}, make: {:?}, model: {:?}, years: &[{ranges}], vin: &{vins:?} }},\n", string("make"), string("model")));
            }
            generated.push_str("];\n");
        }
    }
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("catalog.rs"),
        generated,
    )
    .unwrap();
}
