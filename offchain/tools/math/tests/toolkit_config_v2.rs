use std::{
    fs,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn spawn(config_path: &Path, bind_addr: &str) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_math"))
            .env("NEXUS_TOOLKIT_CONFIG_PATH", config_path)
            .env("BIND_ADDR", bind_addr)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("math binary should start");

        Self(Some(child))
    }

    fn child_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("child should be present")
    }

    fn terminate_and_collect(mut self) -> Output {
        let mut child = self.0.take().expect("child should be present");
        if child
            .try_wait()
            .expect("child status should be readable")
            .is_none()
        {
            child.kill().expect("math binary should terminate");
        }
        child
            .wait_with_output()
            .expect("math binary output should be readable")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn create() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nexus-tools-math-toolkit-config-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("temporary config directory should be created");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn process_failure(message: &str, output: Output) -> String {
    format!(
        "{message}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

#[test]
fn math_binary_accepts_workbench_toolkit_config() {
    let temp_dir = TempDir::create();
    let config_path = temp_dir.0.join("toolkit-config.json");
    fs::write(&config_path, r#"{"signed_http":{"mode":"disabled"}}"#)
        .expect("Workbench-compatible Toolkit config should be written");

    let listener =
        TcpListener::bind(("127.0.0.1", 0)).expect("an available loopback port should exist");
    let bind_addr = listener
        .local_addr()
        .expect("reserved loopback address should be readable");
    drop(listener);

    let mut child = ChildGuard::spawn(&config_path, &bind_addr.to_string());
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        if TcpStream::connect_timeout(&bind_addr, Duration::from_millis(100)).is_ok() {
            break;
        }

        if child
            .child_mut()
            .try_wait()
            .expect("math binary status should be readable")
            .is_some()
        {
            let output = child.terminate_and_collect();
            panic!(
                "{}",
                process_failure(
                    "math binary exited before accepting the Workbench Toolkit config",
                    output,
                )
            );
        }

        if Instant::now() >= deadline {
            let output = child.terminate_and_collect();
            panic!(
                "{}",
                process_failure(
                    "math binary did not listen after accepting the Workbench Toolkit config",
                    output,
                )
            );
        }

        thread::sleep(Duration::from_millis(25));
    }
}
