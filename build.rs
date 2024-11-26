fn main() {
    // Compile protobuf files
    prost_build::compile_protos(
        &["src/protos/chromeos_update_engine/update_metadata.proto"],
        &[] as &[&str],
    )
        .expect("error compiling protobuf files");

    // Windows-specific configuration
    #[cfg(target_os = "windows")]
    {
        // Configure static linking for xz2/lzma
        println!("cargo:rustc-link-search=/usr/x86_64-w64-mingw32/lib");
        println!("cargo:rustc-link-search=/usr/mingw64/lib");
        println!("cargo:rustc-link-lib=static=lzma");

        // Force static linking of C runtime
        println!("cargo:rustc-link-arg=-static-libgcc");
        println!("cargo:rustc-link-arg=-static-libstdc++");

        // Ensure the build script runs if these files change
        println!("cargo:rerun-if-changed=src/protos/chromeos_update_engine/update_metadata.proto");
    }
}