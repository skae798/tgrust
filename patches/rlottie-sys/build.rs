use std::{env, path::PathBuf, process::Command};

fn run(command: &mut Command) {
    let status = command
        .status()
        .expect("Unable to run the rlottie build tool");
    assert!(status.success(), "rlottie build failed: {command:?}");
}

fn main() {
    let source = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("rlottie");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let build = out.join("rlottie-build");
    println!("cargo:rerun-if-changed=rlottie");
    run(Command::new("cmake")
        .arg("-S")
        .arg(&source)
        .arg("-B")
        .arg(&build)
        .args([
            "-DCMAKE_POLICY_VERSION_MINIMUM=3.5",
            "-DCMAKE_BUILD_TYPE=Release",
            "-DBUILD_SHARED_LIBS=OFF",
            "-DCMAKE_POSITION_INDEPENDENT_CODE=ON",
            "-DCMAKE_EXPORT_NO_PACKAGE_REGISTRY=ON",
            "-DLOTTIE_THREAD=OFF",
            "-DLOTTIE_CACHE=OFF",
            "-DLOTTIE_MODULE=OFF",
        ]));
    run(Command::new("cmake").arg("--build").arg(&build).args([
        "--target",
        "rlottie",
        "--parallel",
        "2",
    ]));
    println!("cargo:rustc-link-search=native={}", build.display());
    println!("cargo:rustc-link-lib=static=rlottie");
    println!("cargo:rustc-link-lib=stdc++");
    let include_paths = vec![source.join("inc")];
    println!("cargo:rerun-if-changed=wrapper.h");
    let bindings = bindgen::Builder::default()
        .formatter(bindgen::Formatter::Prettyplease)
        .clang_args(
            include_paths
                .into_iter()
                .map(|path| format!("-I{}", path.display())),
        )
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .newtype_enum(".*")
        .rust_target(bindgen::RustTarget::stable(70, 0).unwrap_or_else(|_| panic!()))
        .size_t_is_usize(true)
        .use_core()
        .generate()
        .expect("Unable to generate bindings");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
