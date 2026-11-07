// Generates FFmpeg/libva bindings against the system headers at build time, so the program
// always matches the installed FFmpeg version.
fn main() {
    let mut include_paths = vec![];
    for lib in ["libavutil", "libavcodec", "libavformat", "libavfilter", "libva"] {
        let l = pkg_config::Config::new().probe(lib).unwrap_or_else(|e| panic!("{lib} not found: {e}"));
        include_paths.extend(l.include_paths);
    }
    let mut b = bindgen::Builder::default()
        .header("ffi/wrapper.h")
        .allowlist_function("av_.*|avcodec_.*|avformat_.*|avfilter_.*|avio_.*|vaSyncSurface")
        .allowlist_type("AV.*|VA.*")
        .allowlist_var("AV_.*|AVERROR.*|AVIO_.*|AVFMT_.*|VA_STATUS_SUCCESS")
        .default_enum_style(bindgen::EnumVariation::Rust { non_exhaustive: false })
        .derive_default(true)
        .layout_tests(false)
        .generate_comments(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for p in include_paths {
        b = b.clang_arg(format!("-I{}", p.display()));
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("ffi.rs");
    b.generate().expect("bindgen failed").write_to_file(out).unwrap();
    println!("cargo:rerun-if-changed=ffi/wrapper.h");
}
