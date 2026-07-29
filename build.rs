use std::env;
use std::path::PathBuf;
use std::process::Command;

fn compile_worker(name: &str, src_rel: &str, out_dir: &PathBuf, manifest: &PathBuf) {
    let src = manifest.join(src_rel);
    let dest = out_dir.join(name);
    println!("cargo:rerun-if-changed={src_rel}");

    // $ORIGIN so packaged workers find bundled libs under /usr/lib/xai-dict/
    let status = Command::new("gcc")
        .args([
            "-O2",
            "-o",
            dest.to_str().unwrap(),
            src.to_str().unwrap(),
            "-Wl,-rpath,$ORIGIN",
            "-lsherpa-onnx-c-api",
            "-lonnxruntime",
            "-lm",
            "-lpthread",
        ])
        .status();

    match status {
        Ok(s) if s.success() => {
            if let Some(profile_dir) = out_dir.ancestors().find(|p| {
                p.file_name()
                    .map(|n| n == "release" || n == "debug")
                    .unwrap_or(false)
            }) {
                let copy_to = profile_dir.join(name);
                let _ = std::fs::copy(&dest, &copy_to);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &copy_to,
                        std::fs::Permissions::from_mode(0o755),
                    );
                }
            }
            println!(
                "cargo:rustc-env={}_PATH={}",
                name.to_ascii_uppercase().replace('-', "_"),
                dest.display()
            );
        }
        Ok(s) => println!("cargo:warning=failed to build {name} (gcc {s})"),
        Err(e) => println!("cargo:warning=gcc missing for {name}: {e}"),
    }
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    compile_worker(
        "qwen3_worker",
        "native/qwen3_worker.c",
        &out_dir,
        &manifest,
    );
    compile_worker(
        "zipformer_worker",
        "native/zipformer_worker.c",
        &out_dir,
        &manifest,
    );
}
