use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let tools_json_path = manifest_dir.join("tools.json");
    let cargo_toml_path = manifest_dir.join("Cargo.toml");
    println!("cargo::rerun-if-changed={}", tools_json_path.display());
    println!("cargo::rerun-if-changed={}", cargo_toml_path.display());

    let tools_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&tools_json_path).unwrap_or_else(|error| {
            panic!("failed to read {}: {error}", tools_json_path.display())
        }),
    )
    .expect("tools.json must be valid JSON");
    let command = tools_json["command"]
        .as_str()
        .expect("tools.json must set command");
    let cargo_toml: toml::Value = toml::from_str(
        &fs::read_to_string(&cargo_toml_path).unwrap_or_else(|error| {
            panic!("failed to read {}: {error}", cargo_toml_path.display())
        }),
    )
    .expect("Cargo.toml must be valid TOML");
    let binary = cargo_toml
        .get("bin")
        .and_then(toml::Value::as_array)
        .and_then(|bins| bins.first())
        .and_then(|bin| bin.get("name"))
        .and_then(toml::Value::as_str);
    assert_eq!(
        binary,
        Some(command),
        "Cargo binary name must match tools.json command"
    );

    let version = env::var("TOOL_FQN_VERSION").unwrap_or_else(|_| "1".to_owned());
    println!("cargo:rustc-env=TOOL_FQN_VERSION={version}");
    println!("cargo:rerun-if-env-changed=TOOL_FQN_VERSION");
}
