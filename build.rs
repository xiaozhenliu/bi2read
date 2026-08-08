fn main() {
    // Compile the Slint UI at build time. The generated Rust module is brought
    // into scope via `slint::include_modules!()` in src/main.rs.
    slint_build::compile("ui/app.slint").expect("Slint UI compilation failed");
}
