//! Path formatting for user-facing output.
//!
//! Windows canonicalization returns extended-length paths (`\\?\C:\project`).
//! They are correct for the filesystem but noisy in a terminal, so every printed
//! path goes through [`display`], which strips the verbatim prefix.

use std::path::Path;

/// Formats a path the way a human expects to read it.
///
/// `\\?\C:\project` becomes `C:\project`, and `\\?\UNC\server\share` becomes
/// `\\server\share`. Other paths are returned unchanged.
pub fn display(path: &Path) -> String {
    let text = path.to_string_lossy();

    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        text.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::display;

    #[test]
    fn verbatim_drive_path_loses_the_prefix() {
        assert_eq!(display(Path::new(r"\\?\C:\project\src")), r"C:\project\src");
    }

    #[test]
    fn verbatim_unc_path_becomes_a_share_path() {
        assert_eq!(
            display(Path::new(r"\\?\UNC\server\share\project")),
            r"\\server\share\project"
        );
    }

    #[test]
    fn plain_paths_are_unchanged() {
        assert_eq!(display(Path::new(r"C:\project")), r"C:\project");
        assert_eq!(
            display(Path::new("/home/user/project")),
            "/home/user/project"
        );
    }
}
