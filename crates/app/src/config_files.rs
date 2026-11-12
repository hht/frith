//! The config files that apply, and the order they apply in (ADR 13).
//!
//! Answers: where on disk are the user's settings?
//! Must not know: what any setting means (`frith_core::config`).
//!
//! Frith's own file wins outright. Without it, the user's Ghostty files are read in Ghostty's
//! order, each on top of the last.
//! @see Ghostty `src/config/Config.zig` `loadDefaultFiles`, `src/config/file_load.zig`

use std::path::{Path, PathBuf};

use frith_core::config::Config;

/// The settings from the files that apply, and one message per problem found in them.
/// `xdg_config_home` is `$XDG_CONFIG_HOME` when set and not empty.
pub fn load(home: &Path, xdg_config_home: Option<&Path>) -> (Config, Vec<String>) {
    let xdg = xdg_config_home.map_or_else(|| home.join(".config"), Path::to_path_buf);
    let frith = xdg.join("frith/config");
    let app_support = home.join("Library/Application Support/com.mitchellh.ghostty");
    let files: Vec<PathBuf> = if frith.exists() {
        vec![frith]
    } else {
        vec![
            xdg.join("ghostty/config"),
            xdg.join("ghostty/config.ghostty"),
            app_support.join("config"),
            app_support.join("config.ghostty"),
        ]
    };
    let mut config = Config::default();
    let mut msgs = Vec::new();
    for path in files.iter().filter(|p| p.exists()) {
        let shown = path.display();
        match std::fs::read(path) {
            Ok(bytes) => {
                // Ghostty reads bytes line by line; a stray byte spoils only its own line.
                let notes = config.read(&String::from_utf8_lossy(&bytes));
                msgs.extend(notes.errors.iter().map(|e| format!("{shown}: {e}")));
                if !notes.ignored.is_empty() {
                    msgs.push(format!("{shown}: not supported yet, ignored: {}", notes.ignored.join(", ")));
                }
            }
            Err(e) => msgs.push(format!("{shown}: cannot read: {e}")),
        }
    }
    (config, msgs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory standing in for `$HOME`.
    fn home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("frith-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(home: &Path, rel: &str, text: impl AsRef<[u8]>) {
        let path = home.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const APP_SUPPORT: &str = "Library/Application Support/com.mitchellh.ghostty";

    #[test]
    fn without_a_frith_file_the_ghostty_files_apply_in_ghosttys_order() {
        let h = home("ghostty");
        put(&h, ".config/ghostty/config", "font-size = 11\nfont-family = A\n");
        put(&h, ".config/ghostty/config.ghostty", "font-size = 12\n");
        put(&h, &format!("{APP_SUPPORT}/config"), "font-size = 14\n");
        put(&h, &format!("{APP_SUPPORT}/config.ghostty"), "font-size = 15\ntheme = nord\n");
        let (c, msgs) = load(&h, None);
        assert_eq!((c.font_size, c.font_family), (15.0, vec!["A".to_string()]));
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].contains("config.ghostty") && msgs[0].contains("theme"), "{msgs:?}");
    }

    #[test]
    fn a_frith_file_replaces_the_ghostty_files() {
        let h = home("frith");
        put(&h, "xdg/frith/config", "font-size = 16\n");
        put(&h, "xdg/ghostty/config", "font-family = A\n");
        put(&h, &format!("{APP_SUPPORT}/config.ghostty"), "font-family = B\n");
        let (c, msgs) = load(&h, Some(&h.join("xdg")));
        assert_eq!((c.font_size, c.font_family), (16.0, vec![]));
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn no_files_give_the_defaults() {
        let h = home("none");
        assert_eq!(load(&h, None), (Config::default(), vec![]));
    }

    #[test]
    fn a_byte_that_is_not_utf8_spoils_only_its_line() {
        // Ghostty reads config files as bytes, line by line.
        let h = home("bad");
        put(&h, ".config/frith/config", b"# caf\xe9\nfont-size = 16\n");
        let (c, msgs) = load(&h, None);
        assert_eq!(c.font_size, 16.0);
        assert!(msgs.is_empty(), "{msgs:?}");
    }
}
