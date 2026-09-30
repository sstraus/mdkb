//! Release binaries must not be tuned to the CPU of the build host.

use toml::Value;

fn target_cpu(config: &Value, triple: &str) -> Option<String> {
    let flags = config
        .get("target")?
        .get(triple)?
        .get("rustflags")?
        .as_array()?;
    let flags: Vec<&str> = flags.iter().filter_map(Value::as_str).collect();
    flags
        .windows(2)
        .find(|w| w[0] == "-C" && w[1].starts_with("target-cpu="))
        .map(|w| w[1].trim_start_matches("target-cpu=").to_string())
}

// Catches: a linux-arm64 release built on the CI runner with target-cpu=native
// (SIGILL on older aarch64 hosts such as Graviton2).
#[test]
fn linux_arm64_release_targets_are_not_tuned_to_the_build_cpu() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/.cargo/config.toml"))
        .expect("read .cargo/config.toml");
    let config: Value = text.parse().expect("parse .cargo/config.toml");

    for triple in ["aarch64-unknown-linux-gnu", "aarch64-unknown-linux-musl"] {
        let cpu = target_cpu(&config, triple);
        assert!(
            cpu.as_deref().is_some_and(|c| c != "native"),
            "{triple} must pin an explicit portable target-cpu, got {cpu:?}"
        );
    }
}
