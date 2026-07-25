fn main() {
    // rust-embed reads ui/dist at compile time; without this, a rebuilt
    // frontend is not re-embedded until src/admin/ui.rs itself changes.
    println!("cargo:rerun-if-changed=ui/dist");
}
