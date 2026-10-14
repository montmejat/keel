//! Finds MuJoCo (a release from github.com/google-deepmind/mujoco, unpacked:
//! `MUJOCO_DIR`, or where `examples/mujoco/setup.sh` puts it), compiles the
//! C shim against its headers with the system's `cc`, and links both.
//!
//! Without MuJoCo, the crate still builds, so the workspace does: its node
//! then says how to get MuJoCo, and does nothing else.

use std::env;
use std::path::PathBuf;
use std::process::Command;

/// The release the shim is written against.
const VERSION: &str = "3.16.0";

fn main() {
    println!("cargo:rustc-check-cfg=cfg(mujoco)");
    println!("cargo:rerun-if-env-changed=MUJOCO_DIR");
    println!("cargo:rerun-if-changed=shim/shim.c");
    let dir = match env::var_os("MUJOCO_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(format!(".cache/keel/mujoco-{VERSION}")),
    };
    println!("cargo:rerun-if-changed={}", dir.display());
    let (include, lib) = (dir.join("include"), dir.join("lib"));
    if !include.join("mujoco/mujoco.h").exists() || !lib.join("libmujoco.so").exists() {
        println!(
            "cargo:warning=keel-mujoco: no MuJoCo in {}: building without it (examples/mujoco/setup.sh fetches it)",
            dir.display()
        );
        return;
    }

    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let object = out.join("shim.o");
    let cc = env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(&cc)
        .args(["-c", "-O2", "-fPIC", "-std=c11", "-Wall", "-Wextra", "-Wno-unused-parameter"])
        .arg("-I")
        .arg(&include)
        .arg("shim/shim.c")
        .arg("-o")
        .arg(&object)
        .status()
        .unwrap_or_else(|e| panic!("can't run the C compiler `{cc}`: {e}"));
    assert!(status.success(), "the C shim doesn't compile");
    let archive = out.join("libkeel_mujoco_shim.a");
    let _ = std::fs::remove_file(&archive);
    let status = Command::new("ar").arg("crs").arg(&archive).arg(&object).status().expect("can't run `ar`");
    assert!(status.success(), "`ar` failed");

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=keel_mujoco_shim");
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=dylib=mujoco");
    println!("cargo:rustc-link-lib=dylib=dl");
    // Found at run time where it was at build time.
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib.display());
    println!("cargo:rustc-cfg=mujoco");
}
