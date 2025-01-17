use std::{
    env,
    ffi::OsString,
    fs,
    io::{BufRead as _, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
};

use cargo_metadata::{
    Artifact, CompilerMessage, Message, Metadata, MetadataCommand, Package, Target,
};
use xtask::{exec, AYA_BUILD_INTEGRATION_BPF, LIBBPF_DIR};

/// This crate has a runtime dependency on artifacts produced by the `integration-ebpf` crate. This
/// would be better expressed as one or more [artifact-dependencies][bindeps] but issues such as:
/// * https://github.com/rust-lang/cargo/issues/12374
/// * https://github.com/rust-lang/cargo/issues/12375
/// * https://github.com/rust-lang/cargo/issues/12385
/// prevent their use for the time being.
///
/// This file, along with the xtask crate, allows analysis tools such as `cargo check`, `cargo
/// clippy`, and even `cargo build` to work as users expect. Prior to this file's existence, this
/// crate's undeclared dependency on artifacts from `integration-ebpf` would cause build (and `cargo check`,
/// and `cargo clippy`) failures until the user ran certain other commands in the workspace. Conversely,
/// those same tools (e.g. cargo test --no-run) would produce stale results if run naively because
/// they'd make use of artifacts from a previous build of `integration-ebpf`.
///
/// Note that this solution is imperfect: in particular it has to balance correctness with
/// performance; an environment variable is used to replace true builds of `integration-ebpf` with
/// stubs to preserve the property that code generation and linking (in `integration-ebpf`) do not
/// occur on metadata-only actions such as `cargo check` or `cargo clippy` of this crate. This means
/// that naively attempting to `cargo test --no-run` this crate will produce binaries that fail at
/// runtime because the stubs are inadequate for actually running the tests.
///
/// [bindeps]: https://doc.rust-lang.org/nightly/cargo/reference/unstable.html?highlight=feature#artifact-dependencies

fn main() {
    println!("cargo:rerun-if-env-changed={}", AYA_BUILD_INTEGRATION_BPF);

    let build_integration_bpf = env::var(AYA_BUILD_INTEGRATION_BPF)
        .as_deref()
        .map(str::parse)
        .map(Result::unwrap)
        .unwrap_or_default();

    let Metadata { packages, .. } = MetadataCommand::new().no_deps().exec().unwrap();
    let integration_ebpf_package = packages
        .into_iter()
        .find(|Package { name, .. }| name == "integration-ebpf")
        .unwrap();

    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let manifest_dir = PathBuf::from(manifest_dir);
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let out_dir = PathBuf::from(out_dir);

    let endian = env::var_os("CARGO_CFG_TARGET_ENDIAN").unwrap();
    let target = if endian == "big" {
        "bpfeb"
    } else if endian == "little" {
        "bpfel"
    } else {
        panic!("unsupported endian={:?}", endian)
    };

    const C_BPF: &[(&str, &str)] = &[
        ("ext.bpf.c", "ext.bpf.o"),
        ("main.bpf.c", "main.bpf.o"),
        ("multimap-btf.bpf.c", "multimap-btf.bpf.o"),
        ("reloc.bpf.c", "reloc.bpf.o"),
        ("text_64_64_reloc.c", "text_64_64_reloc.o"),
    ];

    let c_bpf = C_BPF.iter().map(|(src, dst)| (src, out_dir.join(dst)));

    const C_BTF: &[(&str, &str)] = &[("reloc.btf.c", "reloc.btf.o")];

    let c_btf = C_BTF.iter().map(|(src, dst)| (src, out_dir.join(dst)));

    if build_integration_bpf {
        let libbpf_dir = manifest_dir
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(LIBBPF_DIR);
        println!("cargo:rerun-if-changed={}", libbpf_dir.to_str().unwrap());

        let libbpf_headers_dir = out_dir.join("libbpf_headers");

        let mut includedir = OsString::new();
        includedir.push("INCLUDEDIR=");
        includedir.push(&libbpf_headers_dir);

        exec(
            Command::new("make")
                .arg("-C")
                .arg(libbpf_dir.join("src"))
                .arg(includedir)
                .arg("install_headers"),
        )
        .unwrap();

        let bpf_dir = manifest_dir.join("bpf");

        let mut target_arch = OsString::new();
        target_arch.push("-D__TARGET_ARCH_");

        let arch = env::var_os("CARGO_CFG_TARGET_ARCH").unwrap();
        if arch == "x86_64" {
            target_arch.push("x86");
        } else if arch == "aarch64" {
            target_arch.push("arm64");
        } else {
            target_arch.push(arch);
        };

        for (src, dst) in c_bpf {
            let src = bpf_dir.join(src);
            println!("cargo:rerun-if-changed={}", src.to_str().unwrap());

            exec(
                Command::new("clang")
                    .arg("-I")
                    .arg(&libbpf_headers_dir)
                    .args(["-g", "-O2", "-target", target, "-c"])
                    .arg(&target_arch)
                    .arg(src)
                    .arg("-o")
                    .arg(dst),
            )
            .unwrap();
        }

        for (src, dst) in c_btf {
            let src = bpf_dir.join(src);
            println!("cargo:rerun-if-changed={}", src.to_str().unwrap());

            let mut cmd = Command::new("clang");
            cmd.arg("-I")
                .arg(&libbpf_headers_dir)
                .args(["-g", "-target", target, "-c"])
                .arg(&target_arch)
                .arg(src)
                .args(["-o", "-"]);

            let mut child = cmd
                .stdout(Stdio::piped())
                .spawn()
                .unwrap_or_else(|err| panic!("failed to spawn {cmd:?}: {err}"));

            let Child { stdout, .. } = &mut child;
            let stdout = stdout.take().unwrap();

            let mut output = OsString::new();
            output.push(".BTF=");
            output.push(dst);
            exec(
                // NB: objcopy doesn't support reading from stdin, so we have to use llvm-objcopy.
                Command::new("llvm-objcopy")
                    .arg("--dump-section")
                    .arg(output)
                    .arg("-")
                    .stdin(stdout),
            )
            .unwrap();

            let status = child
                .wait()
                .unwrap_or_else(|err| panic!("failed to wait for {cmd:?}: {err}"));
            match status.code() {
                Some(code) => match code {
                    0 => {}
                    code => panic!("{cmd:?} exited with status code {code}"),
                },
                None => panic!("{cmd:?} terminated by signal"),
            }
        }

        let target = format!("{target}-unknown-none");

        let Package { manifest_path, .. } = integration_ebpf_package;
        let integration_ebpf_dir = manifest_path.parent().unwrap();

        // We have a build-dependency on `integration-ebpf`, so cargo will automatically rebuild us
        // if `integration-ebpf`'s *library* target or any of its dependencies change. Since we
        // depend on `integration-ebpf`'s *binary* targets, that only gets us half of the way. This
        // stanza ensures cargo will rebuild us on changes to the binaries too, which gets us the
        // rest of the way.
        println!("cargo:rerun-if-changed={}", integration_ebpf_dir.as_str());

        let mut cmd = Command::new("cargo");
        cmd.args([
            "build",
            "-Z",
            "build-std=core",
            "--bins",
            "--message-format=json",
            "--release",
            "--target",
            &target,
        ]);

        // Workaround to make sure that the rust-toolchain.toml is respected.
        for key in ["RUSTUP_TOOLCHAIN", "RUSTC"] {
            cmd.env_remove(key);
        }
        cmd.current_dir(integration_ebpf_dir);

        // Workaround for https://github.com/rust-lang/cargo/issues/6412 where cargo flocks itself.
        let ebpf_target_dir = out_dir.join("integration-ebpf");
        cmd.arg("--target-dir").arg(&ebpf_target_dir);

        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|err| panic!("failed to spawn {cmd:?}: {err}"));
        let Child { stdout, stderr, .. } = &mut child;

        // Trampoline stdout to cargo warnings.
        let stderr = stderr.take().unwrap();
        let stderr = BufReader::new(stderr);
        let stderr = std::thread::spawn(move || {
            for line in stderr.lines() {
                let line = line.unwrap();
                println!("cargo:warning={line}");
            }
        });

        let stdout = stdout.take().unwrap();
        let stdout = BufReader::new(stdout);
        let mut executables = Vec::new();
        for message in Message::parse_stream(stdout) {
            #[allow(clippy::collapsible_match)]
            match message.expect("valid JSON") {
                Message::CompilerArtifact(Artifact {
                    executable,
                    target: Target { name, .. },
                    ..
                }) => {
                    if let Some(executable) = executable {
                        executables.push((name, executable.into_std_path_buf()));
                    }
                }
                Message::CompilerMessage(CompilerMessage { message, .. }) => {
                    println!("cargo:warning={message}");
                }
                Message::TextLine(line) => {
                    println!("cargo:warning={line}");
                }
                _ => {}
            }
        }

        let status = child
            .wait()
            .unwrap_or_else(|err| panic!("failed to wait for {cmd:?}: {err}"));
        match status.code() {
            Some(code) => match code {
                0 => {}
                code => panic!("{cmd:?} exited with status code {code}"),
            },
            None => panic!("{cmd:?} terminated by signal"),
        }

        stderr.join().map_err(std::panic::resume_unwind).unwrap();

        for (name, binary) in executables {
            let dst = out_dir.join(name);
            let _: u64 = fs::copy(&binary, &dst)
                .unwrap_or_else(|err| panic!("failed to copy {binary:?} to {dst:?}: {err}"));
        }
    } else {
        for (_src, dst) in c_bpf.chain(c_btf) {
            fs::write(&dst, []).unwrap_or_else(|err| panic!("failed to create {dst:?}: {err}"));
        }

        let Package { targets, .. } = integration_ebpf_package;
        for Target { name, kind, .. } in targets {
            if *kind != ["bin"] {
                continue;
            }
            let dst = out_dir.join(name);
            fs::write(&dst, []).unwrap_or_else(|err| panic!("failed to create {dst:?}: {err}"));
        }
    }
}
