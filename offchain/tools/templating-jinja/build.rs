use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let tools_json_path = manifest_dir.join("tools.json");
    let cargo_toml_path = manifest_dir.join("Cargo.toml");

    println!("cargo::rerun-if-changed={}", tools_json_path.display());
    println!("cargo::rerun-if-changed={}", cargo_toml_path.display());

    let tools_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&tools_json_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", tools_json_path.display())),
    )
    .expect("tools.json must be valid JSON");

    let command = tools_json["command"]
        .as_str()
        .expect("tools.json must have command");

    let cargo_toml: toml::Value = toml::from_str(
        &fs::read_to_string(&cargo_toml_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", cargo_toml_path.display())),
    )
    .expect("Cargo.toml must be valid TOML");

    let bin_name = cargo_toml
        .get("bin")
        .and_then(|b| b.as_array())
        .and_then(|bins| bins.first())
        .and_then(|b| b.get("name"))
        .and_then(|n| n.as_str());

    match bin_name {
        Some(name) if name == command => {}
        Some(name) => panic!(
            "Binary name mismatch: Cargo.toml [[bin]] name = \"{name}\" \
             but tools.json command = \"{command}\". They must match."
        ),
        None => panic!(
            "Cargo.toml has no [[bin]] section but tools.json specifies \
             command = \"{command}\". Add: [[bin]]\nname = \"{command}\""
        ),
    }

    // FQN version is set by CI via Docker build-arg; defaults to "1" locally.
    let version = env::var("TOOL_FQN_VERSION").unwrap_or_else(|_| "1".to_string());
    println!("cargo:rustc-env=TOOL_FQN_VERSION={version}");
    println!("cargo:rerun-if-env-changed=TOOL_FQN_VERSION");
}
