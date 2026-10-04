use std::fs;
use std::path::{Path, PathBuf};

fn collect(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("read bundled PDF assets") {
        let path = entry.expect("asset directory entry").path();
        if path.is_dir() {
            collect(&path, files);
        } else {
            files.push(path);
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=web/pdf-import.js");
    println!("cargo:rerun-if-changed=web/rag-evaluation.mjs");
    println!("cargo:rerun-if-changed=web/vendor/pdfjs");
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let mut files = vec![
        root.join("web/pdf-import.js"),
        root.join("web/rag-evaluation.mjs"),
    ];
    collect(&root.join("web/vendor/pdfjs"), &mut files);
    files.sort();
    let mut generated = format!(
        "static PDF_ASSETS: [(&str, ConsoleAsset); {}] = [\n",
        files.len()
    );
    for file in files {
        let relative = file.strip_prefix(root.join("web")).unwrap();
        let route = format!("/assets/{}", relative.to_str().unwrap().replace('\\', "/"));
        let mime = match file.extension().and_then(|value| value.to_str()) {
            Some("js" | "mjs") => "text/javascript; charset=utf-8",
            Some("json") => "application/json",
            Some("ttf") => "font/ttf",
            Some("pfb" | "bcmap" | "wasm") => "application/octet-stream",
            _ => "text/plain; charset=utf-8",
        };
        generated.push_str(&format!(
            "({route:?}, ConsoleAsset::binary(include_bytes!({:?}), {mime:?})),\n",
            file.to_str().unwrap()
        ));
    }
    generated.push_str("];\n");
    fs::write(
        PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("pdf_assets.rs"),
        generated,
    )
    .expect("write bundled PDF asset map");
}
