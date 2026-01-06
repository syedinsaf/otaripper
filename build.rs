fn main() {
    // Compile protobuf files
    prost_build::compile_protos(
        &["src/protos/chromeos_update_engine/update_metadata.proto"],
        &[] as &[&str],
    )
    .expect("error compiling protobuf files");

    // Ensure the build script runs if the proto changes (all targets)
    println!("cargo:rerun-if-changed=src/protos/chromeos_update_engine/update_metadata.proto");

    // Windows GNU (MinGW) specific configuration
    #[cfg(all(target_os = "windows", target_env = "gnu"))]
    {
        // Configure static linking for xz2/lzma
        println!("cargo:rustc-link-search=/usr/x86_64-w64-mingw32/lib");
        println!("cargo:rustc-link-search=/usr/mingw64/lib");
        println!("cargo:rustc-link-lib=static=lzma");

        // Force static linking of C++/GCC runtime (MinGW only)
        println!("cargo:rustc-link-arg=-static-libgcc");
        println!("cargo:rustc-link-arg=-static-libstdc++");
    }
}
