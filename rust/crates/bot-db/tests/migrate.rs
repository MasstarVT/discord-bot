//! The Prisma-compatible migration runner: embedded files, checksums and the apply plan (offline).

use bot_db::migrate::{AppliedMigration, Migration, checksum, checksum_matches, migrations, plan};
use chrono::Utc;
use sha2::{Digest, Sha256};

fn dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../prisma/migrations")
}

#[test]
fn every_migration_directory_is_embedded_in_name_order() {
    let mut names: Vec<String> = std::fs::read_dir(dir())
        .unwrap()
        .flatten()
        .filter(|e| e.path().join("migration.sql").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let embedded = migrations();
    assert_eq!(embedded.iter().map(|m| m.name).collect::<Vec<_>>(), names);
    assert_eq!(embedded.first().map(|m| m.name), Some("0_init"));
    assert!(embedded.len() >= 13);
    for m in &embedded {
        let on_disk = std::fs::read(dir().join(m.name).join("migration.sql")).unwrap();
        assert_eq!(m.sql.as_bytes(), on_disk.as_slice(), "{}", m.name);
    }
}

#[test]
fn checksums_are_sha256_hex_of_the_file_like_prisma() {
    for m in migrations() {
        let bytes = std::fs::read(dir().join(m.name).join("migration.sql")).unwrap();
        let expected = hex::encode(Sha256::digest(&bytes));
        assert_eq!(m.checksum(), expected, "{}", m.name);
        assert_eq!(m.checksum().len(), 64);
        assert!(checksum_matches(m.sql, &expected));
    }
    // Known vector.
    assert_eq!(checksum(""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
}

#[test]
fn checksums_tolerate_line_endings_and_the_old_unpadded_format() {
    let lf = "CREATE TABLE \"A\" (\n    \"id\" TEXT NOT NULL\n);\n";
    let crlf = lf.replace('\n', "\r\n");
    // Stored by a Linux deploy (LF), checked against a Windows checkout (CRLF) and the other way round.
    assert!(checksum_matches(&crlf, &checksum(lf)));
    assert!(checksum_matches(lf, &checksum(&crlf)));
    assert!(checksum_matches(lf, &checksum(lf).to_uppercase()));
    assert!(!checksum_matches(lf, &checksum("something else")));
    // Real files: both line-ending variants of every migration match the stored checksum of the other.
    for m in migrations() {
        let as_lf = m.sql.replace("\r\n", "\n");
        let as_crlf = as_lf.replace('\n', "\r\n");
        assert!(checksum_matches(m.sql, &checksum(&as_lf)), "{}", m.name);
        assert!(checksum_matches(m.sql, &checksum(&as_crlf)), "{}", m.name);
    }
    // Old Prisma rendered bytes with `{:x}` (no zero padding).
    let script = (0..200).map(|i| format!("-- {i}\n")).find(|s| Sha256::digest(s.as_bytes()).iter().any(|b| *b < 0x10)).unwrap();
    let unpadded: String = Sha256::digest(script.as_bytes()).iter().map(|b| format!("{b:x}")).collect();
    assert!(unpadded.len() < 64);
    assert!(checksum_matches(&script, &unpadded));
}

fn row(name: &str, checksum: &str, finished: bool, rolled_back: bool) -> AppliedMigration {
    AppliedMigration {
        id: bot_db::new_uuid(),
        checksum: checksum.to_string(),
        finished_at: finished.then(Utc::now),
        migration_name: name.to_string(),
        logs: None,
        rolled_back_at: rolled_back.then(Utc::now),
        started_at: Utc::now(),
        applied_steps_count: i32::from(finished),
    }
}

fn prisma_rows(ms: &[Migration]) -> Vec<AppliedMigration> {
    ms.iter().map(|m| row(m.name, &m.checksum(), true, false)).collect()
}

#[test]
fn a_database_migrated_by_prisma_has_nothing_to_do() {
    let ms = migrations();
    let p = plan(&ms, &prisma_rows(&ms)).unwrap();
    assert!(p.pending.is_empty());
    assert!(p.missing_locally.is_empty());
    // Stored with LF line endings (a Linux deploy) while this checkout may have CRLF.
    let lf_rows: Vec<_> = ms.iter().map(|m| row(m.name, &checksum(&m.sql.replace("\r\n", "\n")), true, false)).collect();
    assert!(plan(&ms, &lf_rows).unwrap().pending.is_empty());
}

#[test]
fn an_empty_database_gets_everything_in_order() {
    let ms = migrations();
    let p = plan(&ms, &[]).unwrap();
    assert_eq!(p.pending, ms);
    // Input order does not matter.
    let mut reversed = ms.clone();
    reversed.reverse();
    assert_eq!(plan(&reversed, &[]).unwrap().pending, ms);
}

#[test]
fn only_unrecorded_migrations_are_applied() {
    let ms = migrations();
    let mut rows = prisma_rows(&ms[..ms.len() - 2]);
    // A rolled-back attempt of the next one does not count as applied.
    rows.push(row(ms[ms.len() - 2].name, "whatever", false, true));
    let p = plan(&ms, &rows).unwrap();
    assert_eq!(p.pending, ms[ms.len() - 2..].to_vec());
}

#[test]
fn a_failed_migration_stops_the_run() {
    let ms = migrations();
    let mut rows = prisma_rows(&ms[..3]);
    rows.push(row(ms[3].name, &ms[3].checksum(), false, false));
    let err = plan(&ms, &rows).unwrap_err().to_string();
    assert!(err.contains("P3009"), "{err}");
    assert!(err.contains(ms[3].name), "{err}");
}

#[test]
fn an_edited_migration_is_a_warning_like_prisma() {
    let ms = migrations();
    let mut rows = prisma_rows(&ms);
    rows[1].checksum = checksum("-- edited");
    let p = plan(&ms, &rows).expect("a modified migration does not stop the run");
    assert!(p.pending.is_empty());
    assert_eq!(p.modified, vec![ms[1].name.to_string()]);
    assert_eq!(
        bot_db::migrate::modified_warning(ms[1].name),
        format!("The migration `{}` was modified after it was applied.", ms[1].name)
    );
}

#[test]
fn migrations_unknown_to_this_build_are_reported_not_fatal() {
    let ms = migrations();
    let mut rows = prisma_rows(&ms);
    rows.push(row("29991231000000_future", "abc", true, false));
    let p = plan(&ms, &rows).unwrap();
    assert!(p.pending.is_empty());
    assert_eq!(p.missing_locally, vec!["29991231000000_future".to_string()]);
}

#[test]
fn migrations_table_ddl_matches_prisma() {
    let ddl = bot_db::migrate::CREATE_MIGRATIONS_TABLE_SQL;
    for col in [
        "id                      VARCHAR(36) PRIMARY KEY NOT NULL",
        "checksum                VARCHAR(64) NOT NULL",
        "finished_at             TIMESTAMPTZ",
        "migration_name          VARCHAR(255) NOT NULL",
        "logs                    TEXT",
        "rolled_back_at          TIMESTAMPTZ",
        "started_at              TIMESTAMPTZ NOT NULL DEFAULT now()",
        "applied_steps_count     INTEGER NOT NULL DEFAULT 0",
    ] {
        assert!(ddl.contains(col), "{col}");
    }
    assert_eq!(bot_db::migrate::PRISMA_ADVISORY_LOCK_KEY, 72707369);
}
