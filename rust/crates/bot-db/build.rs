//! Embeds every `prisma/migrations/<name>/migration.sql` (sorted by directory name, like Prisma) at compile time.

use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let dir = manifest.join("..").join("..").join("..").join("prisma").join("migrations");
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(read) = fs::read_dir(&dir) {
        for entry in read.flatten() {
            let path = entry.path();
            let sql = path.join("migration.sql");
            if path.is_dir() && sql.is_file() {
                println!("cargo:rerun-if-changed={}", sql.display());
                entries.push((entry.file_name().to_string_lossy().into_owned(), sql));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::from("/// `(name, migration.sql)` of every Prisma migration, sorted by name.\n");
    out.push_str("pub static EMBEDDED: &[(&str, &str)] = &[\n");
    for (name, path) in &entries {
        out.push_str(&format!("    ({name:?}, include_str!({:?})),\n", path.display().to_string()));
    }
    out.push_str("];\n");
    out.push_str(&format!("/// The migrations directory at build time.\npub const SOURCE_DIR: &str = {:?};\n", dir.display().to_string()));

    let out_path = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("migrations.rs");
    fs::write(out_path, out).expect("write migrations.rs");
}
