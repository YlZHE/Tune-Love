use std::{env, fs, path::PathBuf, process::Command};
#[path = "native/build_options.rs"]
mod build_options;

fn run(command: &mut Command, description: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to start {description}: {error}"));
    if !status.success() {
        panic!("{description} failed with {status}");
    }
}

fn cmake_executable(manifest: &std::path::Path) -> PathBuf {
    let local = manifest.join("vendor/tools/cmake-3.30.5-windows-x86_64/bin/cmake.exe");
    if !local.is_file() {
        panic!(
            "required pinned CMake 3.30.5 is missing at {}; restore the retained verified toolchain",
            local.display()
        );
    }
    local
}

fn build_native_keyfinder(native_opt: Option<u32>) {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let vendor = manifest.join("vendor/key_detection");
    let fftw_source = vendor.join("fftw-3.3.10");
    let keyfinder_source = vendor.join("libkeyfinder-a409c7447e9f440a12627ff4a540a43e41b48a55/src");
    let fftw_build = out.join("fftw-build");
    let fftw_install = out.join("fftw-install");
    fs::create_dir_all(&fftw_build).expect("create FFTW build directory");
    let cmake = cmake_executable(&manifest);
    run(
        Command::new(&cmake)
            .args(["-S"])
            .arg(&fftw_source)
            .args(["-B"])
            .arg(&fftw_build)
            .args(["-G", "Visual Studio 17 2022", "-A", "x64"])
            .args([
                "-DBUILD_SHARED_LIBS=OFF",
                "-DBUILD_TESTS=OFF",
                "-DENABLE_THREADS=OFF",
                "-DENABLE_OPENMP=OFF",
                "-DENABLE_FLOAT=OFF",
            ])
            .arg(format!("-DCMAKE_INSTALL_PREFIX={}", fftw_install.display())),
        "configure pinned FFTW 3.3.10",
    );
    run(
        Command::new(&cmake)
            .args(["--build"])
            .arg(&fftw_build)
            .args(["--config", "Release", "--target", "INSTALL"]),
        "build pinned FFTW 3.3.10",
    );

    let sources = [
        "audiodata.cpp",
        "chromagram.cpp",
        "chromatransform.cpp",
        "chromatransformfactory.cpp",
        "fftadapter.cpp",
        "keyclassifier.cpp",
        "keyfinder.cpp",
        "lowpassfilter.cpp",
        "lowpassfilterfactory.cpp",
        "spectrumanalyser.cpp",
        "temporalwindowfactory.cpp",
        "toneprofiles.cpp",
        "windowfunctions.cpp",
        "workspace.cpp",
        "constants.cpp",
    ];
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .include(&keyfinder_source)
        .include(fftw_install.join("include"))
        .file(manifest.join("native/keyfinder_bridge.cpp"))
        .flag_if_supported("/EHsc")
        .flag_if_supported("/std:c++14");
    for source in sources {
        build.file(keyfinder_source.join(source));
    }
    if let Some(level) = native_opt {
        build.opt_level(level);
    }
    build.compile("keyfinder_native");

    println!(
        "cargo:rustc-link-search=native={}",
        fftw_install.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=fftw3");
    println!("cargo:rerun-if-changed=native/keyfinder_bridge.cpp");
    println!(
        "cargo:rerun-if-changed={}",
        vendor.join("fftw-3.3.10").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        vendor
            .join("libkeyfinder-a409c7447e9f440a12627ff4a540a43e41b48a55/src")
            .display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest
            .join("vendor/tools/cmake-3.30.5-windows-x86_64/bin/cmake.exe")
            .display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest
            .join("vendor/tools/cmake-3.30.5-windows-x86_64/share/cmake-3.30/Modules")
            .display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest
            .join("vendor/tools/cmake-3.30.5-SHA-256.txt")
            .display()
    );
    println!("cargo:rerun-if-changed=build.rs");
}

fn main() {
    println!("cargo:rerun-if-env-changed=KEYFINDER_NATIVE_OPT_LEVEL");
    println!("cargo:rerun-if-changed=native/build_options.rs");
    let requested = match env::var("KEYFINDER_NATIVE_OPT_LEVEL") {
        Ok(v) => Some(v),
        Err(env::VarError::NotPresent) => None,
        Err(_) => panic!("KEYFINDER_NATIVE_OPT_LEVEL is not valid Unicode"),
    };
    let native_opt =
        build_options::parse_native_opt(requested.as_deref()).expect("native optimization setting");
    println!(
        "cargo:rustc-env=KEYFINDER_NATIVE_OPT_REQUESTED={}",
        requested.as_deref().unwrap_or("default")
    );
    println!(
        "cargo:rustc-env=KEYFINDER_NATIVE_OPT_EFFECTIVE={}",
        requested
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| env::var("OPT_LEVEL").expect("OPT_LEVEL"))
    );
    println!(
        "cargo:rustc-env=KEYFINDER_RUST_PROFILE={}",
        env::var("PROFILE").expect("PROFILE")
    );
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        build_native_keyfinder(native_opt);
    }
    tauri_build::build();
}
