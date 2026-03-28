//! Build script: ensures Linux binaries (k3dev-agent, socat) exist in assets/.
//! On macOS, these run inside the k3s Linux container, so we cross-compile via Docker.
//! On Linux, we compile natively with musl.

use std::path::{Path, PathBuf};
use std::process::Command;

const SOCAT_VERSION: &str = "1.8.0.3";

fn main() {
    let project_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let assets_dir = project_dir.join("assets");
    let agent_dir = project_dir.join("agent");

    std::fs::create_dir_all(&assets_dir).unwrap();

    // Determine which architectures we need
    let host_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let arches = ["aarch64", "x86_64"];

    for arch in &arches {
        let agent_path = assets_dir.join(format!("k3dev-agent-{}", arch));
        let socat_path = assets_dir.join(format!("socat-{}", arch));

        if *arch == host_arch {
            // Build real binaries for the host architecture
            if needs_build(&agent_path) {
                build_agent(&agent_dir, &assets_dir, arch);
            }
            if needs_build(&socat_path) {
                build_socat(&assets_dir, arch);
            }
        } else {
            // Create placeholder for non-host arch (guarded by #[cfg(target_arch)])
            if !agent_path.exists() {
                std::fs::write(&agent_path, "placeholder").unwrap();
            }
            if !socat_path.exists() {
                std::fs::write(&socat_path, "placeholder").unwrap();
            }
        }

        // Rerun if assets change
        println!("cargo:rerun-if-changed={}", agent_path.display());
        println!("cargo:rerun-if-changed={}", socat_path.display());
    }

    println!("cargo:rerun-if-changed=agent/src/main.rs");
    println!("cargo:rerun-if-changed=agent/Cargo.toml");
}

/// Check if a binary needs to be (re)built: missing, placeholder, or not a valid ELF.
fn needs_build(path: &Path) -> bool {
    let Ok(data) = std::fs::read(path) else {
        return true;
    };
    // ELF magic: 0x7f ELF
    if data.len() < 4 || &data[..4] != b"\x7fELF" {
        return true;
    }
    false
}

fn build_agent(agent_dir: &Path, assets_dir: &Path, arch: &str) {
    let target = format!("{}-unknown-linux-musl", arch);
    let dest = assets_dir.join(format!("k3dev-agent-{}", arch));

    println!("cargo:warning=Building k3dev-agent for {} ...", arch);

    // Try Docker-based build (works on macOS and Linux without musl toolchain)
    if try_docker_build_agent(agent_dir, assets_dir, arch, &target) {
        return;
    }

    // Fallback: native musl build (Linux with musl-tools installed)
    let _ = Command::new("rustup")
        .args(["target", "add", &target])
        .status();

    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            &target,
            "--manifest-path",
            agent_dir.join("Cargo.toml").to_str().unwrap(),
        ])
        .status();

    if let Ok(s) = status {
        if s.success() {
            let built = agent_dir
                .join("target")
                .join(&target)
                .join("release")
                .join("k3dev-agent");
            if built.exists() {
                std::fs::copy(&built, &dest).unwrap();
                println!("cargo:warning=Built k3dev-agent-{} (native musl)", arch);
                return;
            }
        }
    }

    panic!(
        "Failed to build k3dev-agent-{}. Install Docker or musl-tools.",
        arch
    );
}

fn try_docker_build_agent(
    agent_dir: &Path,
    assets_dir: &Path,
    arch: &str,
    _target: &str,
) -> bool {
    // Check Docker is available
    if Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_or(false, |s| s.success())
        == false
    {
        return false;
    }

    let dest = assets_dir.join(format!("k3dev-agent-{}", arch));
    let platform = match arch {
        "aarch64" => "linux/arm64",
        "x86_64" => "linux/amd64",
        _ => return false,
    };

    let status = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--platform",
            platform,
            "-v",
            &format!("{}:/work", agent_dir.display()),
            "-v",
            &format!("{}:/out", assets_dir.display()),
            "-w",
            "/work",
            "rust:1.86-alpine",
            "sh",
            "-c",
            &format!(
                "apk add -q musl-dev && \
                 cargo build --release && \
                 strip target/release/k3dev-agent && \
                 cp target/release/k3dev-agent /out/k3dev-agent-{}",
                arch
            ),
        ])
        .status();

    if let Ok(s) = status {
        if s.success() && dest.exists() && needs_build(&dest) == false {
            println!("cargo:warning=Built k3dev-agent-{} (Docker)", arch);
            return true;
        }
    }

    false
}

fn build_socat(assets_dir: &Path, arch: &str) {
    println!("cargo:warning=Building socat {} for {} ...", SOCAT_VERSION, arch);

    // Try Docker-based build (works on macOS and Linux)
    if try_docker_build_socat(assets_dir, arch) {
        return;
    }

    // Fallback: native build (Linux only)
    if cfg!(target_os = "linux") && try_native_build_socat(assets_dir, arch) {
        return;
    }

    panic!(
        "Failed to build socat-{}. Install Docker or build dependencies (gcc, musl-tools, autoconf).",
        arch
    );
}

fn try_docker_build_socat(assets_dir: &Path, arch: &str) -> bool {
    if Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_or(false, |s| s.success())
        == false
    {
        return false;
    }

    let dest = assets_dir.join(format!("socat-{}", arch));
    let platform = match arch {
        "aarch64" => "linux/arm64",
        "x86_64" => "linux/amd64",
        _ => return false,
    };

    let status = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--platform",
            platform,
            "-v",
            &format!("{}:/out", assets_dir.display()),
            "alpine:3.21",
            "sh",
            "-c",
            &format!(
                "apk add -q build-base linux-headers && \
                 wget -q http://www.dest-unreach.org/socat/download/socat-{version}.tar.gz -O /tmp/socat.tar.gz && \
                 cd /tmp && tar xzf socat.tar.gz && cd socat-{version} && \
                 CFLAGS='-static -Os' LDFLAGS='-static -s' ./configure \
                   --disable-openssl --disable-readline --disable-libwrap --disable-fips \
                   >/dev/null 2>&1 && \
                 make -j$(nproc) >/dev/null 2>&1 && \
                 strip socat && \
                 cp socat /out/socat-{arch}",
                version = SOCAT_VERSION,
                arch = arch
            ),
        ])
        .status();

    if let Ok(s) = status {
        if s.success() && dest.exists() && !needs_build(&dest) {
            println!("cargo:warning=Built socat-{} (Docker)", arch);
            return true;
        }
    }

    false
}

fn try_native_build_socat(assets_dir: &Path, arch: &str) -> bool {
    let dest = assets_dir.join(format!("socat-{}", arch));
    let tmpdir = std::env::temp_dir().join(format!("k3dev-socat-build-{}", arch));
    let _ = std::fs::remove_dir_all(&tmpdir);
    std::fs::create_dir_all(&tmpdir).unwrap();

    // Download
    let dl = Command::new("curl")
        .args([
            "-sSfL",
            &format!(
                "http://www.dest-unreach.org/socat/download/socat-{}.tar.gz",
                SOCAT_VERSION
            ),
            "-o",
            tmpdir.join("socat.tar.gz").to_str().unwrap(),
        ])
        .status();
    if dl.map_or(true, |s| !s.success()) {
        return false;
    }

    // Extract and build
    let build_script = format!(
        "cd {tmp} && tar xzf socat.tar.gz && cd socat-{ver} && \
         CC=musl-gcc CFLAGS='-static -Os' LDFLAGS='-static -s' ./configure \
           --disable-openssl --disable-readline --disable-libwrap --disable-fips \
           >/dev/null 2>&1 && \
         make -j$(nproc) >/dev/null 2>&1 && \
         strip socat && \
         cp socat {dest}",
        tmp = tmpdir.display(),
        ver = SOCAT_VERSION,
        dest = dest.display()
    );

    let status = Command::new("sh").args(["-c", &build_script]).status();
    let _ = std::fs::remove_dir_all(&tmpdir);

    if let Ok(s) = status {
        if s.success() && dest.exists() && !needs_build(&dest) {
            println!("cargo:warning=Built socat-{} (native musl)", arch);
            return true;
        }
    }

    false
}
