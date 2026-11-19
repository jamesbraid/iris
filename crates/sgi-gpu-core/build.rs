// Marks a build of the crate itself, as opposed to iris compiling the same files.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(sgi_gpu_core)");
    println!("cargo::rustc-cfg=sgi_gpu_core");
}
