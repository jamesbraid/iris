//! Prints the C declarations cbindgen makes of `src/ffi.rs`, for
//! `tools/check-header`.

fn main() {
    // The host and configuration structures are used by the boards' `new`, not here.
    let export = cbindgen::ExportConfig { include: vec!["IrisEngineHost".into(), "IrisEngineConfig".into()], ..Default::default() };
    let config = cbindgen::Config {
        language: cbindgen::Language::C,
        style: cbindgen::Style::Type,
        usize_is_size_t: true,
        export,
        ..Default::default()
    };
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/src/ffi.rs");
    cbindgen::Builder::new().with_config(config).with_src(src).generate().expect("cbindgen").write(std::io::stdout());
}
