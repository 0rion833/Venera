//! CLI path output: canonical paths must not leak the Windows verbatim prefix.

use std::error::Error;

#[test]
fn index_prints_paths_without_the_verbatim_prefix() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("a.txt"), "hello")?;

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_venera"))
        .args(["index", "."])
        .current_dir(dir.path())
        .output()?;

    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains(r"\\?\"),
        "verbatim prefix leaked into output: {stdout}"
    );

    let name = dir
        .path()
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let root = stdout
        .lines()
        .find(|line| line.starts_with("root"))
        .unwrap_or_default();
    assert!(root.contains(&name), "unexpected root line: {root}");

    let database = stdout
        .lines()
        .find(|line| line.starts_with("database"))
        .unwrap_or_default();
    assert!(
        database.ends_with(".venera\\index.db") || database.ends_with(".venera/index.db"),
        "unexpected database line: {database}"
    );
    Ok(())
}
