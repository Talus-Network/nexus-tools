use std::process::Command;

#[test]
fn worker_configuration_failure_exits_nonzero() {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-api"))
        .arg("worker")
        .env_clear()
        .output()
        .expect("run the agent-api worker subprocess");

    assert!(
        !output.status.success(),
        "worker startup failure returned success; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("agent-api worker could not start"),
        "worker startup failure must retain non-secret process context"
    );
}
