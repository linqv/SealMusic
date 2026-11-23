use std::process::Command;

#[test]
fn offline_protocol() {
    let status = Command::new("python3")
        .args(["tests/protocol.py", "--binary", env!("CARGO_BIN_EXE_lx-probe")])
        .status()
        .expect("run the loopback protocol suite");
    assert!(status.success());
}
