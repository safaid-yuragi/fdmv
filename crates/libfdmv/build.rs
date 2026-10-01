//! libopus の場所を pkg-config で探し、リンカの検索パスに加える。
//!
//! opusic-sys（システムの libopus を使う設定）は `-lopus` だけを出力するので、
//! Homebrew（/opt/homebrew/lib）や MSYS2 など標準以外の場所にあると見つからない。
//! pkg-config で見つからなければ何もしない（opusic-sys の `OPUS_LIB_DIR` も使える）。

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(feature = "decode")]
    if std::env::var_os("CARGO_FEATURE_BUNDLED_OPUS").is_none() {
        if let Ok(lib) = pkg_config::Config::new()
            .cargo_metadata(false)
            .probe("opus")
        {
            for dir in lib.link_paths {
                println!("cargo:rustc-link-search=native={}", dir.display());
            }
        }
    }
}
