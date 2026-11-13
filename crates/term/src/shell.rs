//! Shell integration (ADR 11): the scripts that make a shell mark its prompts with OSC 133,
//! so the terminal clears a prompt on resize and the shell redraws it, instead of the old
//! prompt staying on screen.
//!
//! Answers: how does a shell Frith starts load Frith's prompt marks?
//! Must not know: terminals, PTYs, or what the marks do on screen.
//!
//! The scripts are compiled in and written once to a directory named by their content, so
//! a new version never runs an old script and concurrent instances never see a half-written
//! one. zsh loads them through ZDOTDIR, as Ghostty does; the script restores the user's
//! ZDOTDIR and runs the user's .zshenv first.
//! @see Ghostty `src/termio/shell_integration.zig` (`setupZsh`)

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Scripts by path under the integration directory.
const FILES: [(&str, &str); 1] = [("zsh/.zshenv", include_str!("../shell-integration/zsh/.zshenv"))];

/// Where zsh finds the user's ZDOTDIR while ours is in effect; unset when the user had none.
const SAVED_ZDOTDIR: &str = "FRITH_ZSH_ZDOTDIR";

/// Writes the scripts under `cache` unless this version is already there, and returns the
/// integration directory. Cheap when installed, so it runs before every shell starts: a
/// cache cleaner may have removed the scripts, and a ZDOTDIR without them would make zsh
/// skip the user's startup files.
pub fn install(cache: &Path) -> io::Result<PathBuf> {
    let parent = cache.join("shell-integration");
    let dir = parent.join(format!("{:016x}", content_hash()));
    if FILES.iter().all(|(name, _)| dir.join(name).is_file()) {
        return Ok(dir);
    }
    let _ = fs::remove_dir_all(&dir);
    let tmp = parent.join(format!(".tmp-{}-{:016x}", std::process::id(), content_hash()));
    let _ = fs::remove_dir_all(&tmp);
    for (name, text) in FILES {
        let path = tmp.join(name);
        fs::create_dir_all(path.parent().expect("script paths have a directory"))?;
        fs::write(path, text)?;
    }
    // A complete directory appears at once; another instance may have renamed its own first.
    if let Err(e) = fs::rename(&tmp, &dir) {
        let _ = fs::remove_dir_all(&tmp);
        if !dir.is_dir() {
            return Err(e);
        }
    }
    Ok(dir)
}

/// The variables that make `shell` load the integration in `dir`, given the user's own
/// ZDOTDIR. Only zsh is integrated so far (ADR 11).
pub(crate) fn env(shell: &Path, dir: &Path, zdotdir: Option<OsString>) -> Vec<(&'static str, OsString)> {
    if shell.file_name() != Some(OsStr::new("zsh")) {
        return Vec::new();
    }
    let mut vars = vec![("ZDOTDIR", dir.join("zsh").into_os_string())];
    if let Some(old) = zdotdir {
        vars.push((SAVED_ZDOTDIR, old));
    }
    vars
}

/// The per-user cache directory Frith may write to, on macOS; other platforms come with
/// their ports.
pub fn cache_dir(home: &Path) -> PathBuf {
    home.join("Library/Caches/frith")
}

/// FNV-1a over every script's path and text: stable across builds, unlike std's hasher.
/// @see http://www.isthe.com/chongo/tech/comp/fnv/index.html
fn content_hash() -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (name, text) in FILES {
        for b in name.bytes().chain([0]).chain(text.bytes()).chain([0]) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh directory under the system's temp directory.
    pub(crate) fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("frith-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn installing_writes_the_scripts_once_into_a_directory_named_by_their_content() {
        let cache = scratch("install");
        let dir = install(&cache).unwrap();
        let zshenv = dir.join("zsh/.zshenv");
        assert_eq!(fs::read_to_string(&zshenv).unwrap(), FILES[0].1);
        fs::write(&zshenv, "edited").unwrap();
        assert_eq!(install(&cache).unwrap(), dir, "same version, same directory");
        assert_eq!(fs::read_to_string(&zshenv).unwrap(), "edited", "an installed version is not rewritten");
        let left: Vec<_> = fs::read_dir(cache.join("shell-integration")).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary directory is left behind");
        // A cache cleaner removed the script: without it zsh would skip the user's files.
        fs::remove_file(&zshenv).unwrap();
        assert_eq!(install(&cache).unwrap(), dir);
        assert_eq!(fs::read_to_string(&zshenv).unwrap(), FILES[0].1, "a missing script is written again");
    }

    #[test]
    fn zsh_gets_our_zdotdir_and_keeps_the_users_for_the_script_to_restore() {
        let dir = Path::new("/cache/si");
        let ours = || ("ZDOTDIR", OsString::from("/cache/si/zsh"));
        assert_eq!(env(Path::new("/bin/zsh"), dir, None), vec![ours()]);
        assert_eq!(
            env(Path::new("/opt/homebrew/bin/zsh"), dir, Some("/u/.config/zsh".into())),
            vec![ours(), (SAVED_ZDOTDIR, OsString::from("/u/.config/zsh"))]
        );
        assert_eq!(env(Path::new("/bin/bash"), dir, Some("/x".into())), vec![], "only zsh so far");
    }
}
