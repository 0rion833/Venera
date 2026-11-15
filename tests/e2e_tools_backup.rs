use std::error::Error;
use std::fs;

use serde_json::json;

use venera::security::permissions::Permissions;
use venera::vce::backup::{self, BackupStore};
use venera::vce::index::query::Engine;
use venera::vce::index::refresh::RefreshOptions;
use venera::vce::tools::registry::Dispatcher;

#[test]
fn user_backup_survives_ai_edits_and_restores() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let notes = root.join("notes.txt");
    fs::write(&notes, "test")?;

    let engine = Engine::open(root, ".venera/index.db")?;
    engine.refresh(&RefreshOptions::default())?;
    let dispatcher = Dispatcher::new(Permissions::permissive());

    let first = backup::snapshot(&engine)?;
    println!("1. snapshot: files={}, saved={}", first.files, first.saved);
    assert_eq!(first.saved, 1);

    fs::write(&notes, "test\nuser line")?;
    let synced = backup::sync(&engine, &["notes.txt".to_owned()])?;
    println!("2. user edit synced: {synced}");
    assert_eq!(synced, 1);

    let appended = dispatcher.call(
        &engine,
        "append_file",
        &json!({"path": "notes.txt", "content": "ai line"}),
    )?;
    println!("3. AI append: {}", appended.text);
    assert!(fs::read_to_string(&notes)?.contains("ai line"));

    let after_ai = backup::snapshot(&engine)?;
    println!(
        "4. snapshot after AI: files={}, skipped_ai={}",
        after_ai.files, after_ai.skipped_ai
    );
    let rows = BackupStore::open(root)?.rows()?;
    let saved_notes = rows
        .iter()
        .find(|row| row.path == "notes.txt")
        .expect("notes snapshot");
    assert_eq!(
        String::from_utf8(saved_notes.content.clone())?,
        "test\nuser line"
    );

    let created = dispatcher.call(
        &engine,
        "write_file",
        &json!({"path": "ai-created.txt", "content": "only ai"}),
    )?;
    println!("5. AI-created file: {}", created.text);
    assert!(root.join("ai-created.txt").exists());

    backup::snapshot(&engine)?;
    let restored = backup::restore(&engine)?;
    println!("6. restore: {restored:?}");
    assert_eq!(fs::read_to_string(&notes)?, "test\nuser line");
    assert!(!root.join("ai-created.txt").exists());

    let command = if cfg!(windows) {
        "echo e2e-ok"
    } else {
        "printf e2e-ok"
    };
    let shell = dispatcher.call(&engine, "bash", &json!({"command": command}))?;
    println!("7. bash:\n{}", shell.text);
    assert!(shell.text.contains("e2e-ok"));
    assert!(shell.text.contains("exit: 0"));

    Ok(())
}
