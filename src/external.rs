//! External commands: `fhir-pipes NAME ARGS…`, where NAME is not a built-in
//! command, runs the program `fhir-pipes-NAME` with ARGS, as git and cargo
//! run theirs. It is looked for in the folder of the running fhir-pipes,
//! then on the PATH. A connector written in any language can be one (see
//! docs/connectors.md).
//!
//! `--profile`, the one option fhir-pipes reads before a command, reaches
//! the program as `$FHIR_PIPES_PROFILE`, the variable `--profile` already
//! falls back to.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The environment variable that names the profile.
const PROFILE_ENV: &str = "FHIR_PIPES_PROFILE";

/// The command line split at an unknown top-level command: its name, the
/// arguments after it, and the `--profile` given before it.
#[derive(Debug, PartialEq, Eq)]
pub struct Call {
    pub name: String,
    pub args: Vec<OsString>,
    pub profile: Option<OsString>,
}

/// The external command `argv` (without the program name) calls, when its
/// first command is `name`. Before the command, only `--profile` can come:
/// any other option, or a first command other than `name` (an unknown
/// subcommand of a built-in one), gives `None`.
pub fn call(argv: &[OsString], name: &str) -> Option<Call> {
    if !plain_name(name) {
        return None;
    }
    let mut profile = None;
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].to_str();
        match arg {
            Some("--profile") => {
                profile = Some(argv.get(i + 1)?.clone());
                i += 2;
            }
            Some(a) if a.starts_with("--profile=") => {
                profile = Some(OsString::from(&a["--profile=".len()..]));
                i += 1;
            }
            Some(a) if a == name => {
                return Some(Call {
                    name: name.to_string(),
                    args: argv[i + 1..].to_vec(),
                    profile,
                });
            }
            _ => return None,
        }
    }
    None
}

/// Letters, digits, `-` and `_`, not starting with `-`: a name that cannot
/// reach outside the folders searched.
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The program `fhir-pipes-NAME`: in the folder of the running executable,
/// then in each folder of `path` (the PATH). On Windows each name is also
/// tried with the extensions in `$PATHEXT`.
pub fn find(name: &str, exe_dir: Option<&Path>, path: Option<&OsStr>) -> Option<PathBuf> {
    let file = format!("fhir-pipes-{name}");
    let dirs = exe_dir
        .map(Path::to_path_buf)
        .into_iter()
        .chain(path.into_iter().flat_map(std::env::split_paths));
    for dir in dirs {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for candidate in candidates(&dir, &file) {
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn candidates(dir: &Path, file: &str) -> Vec<PathBuf> {
    vec![dir.join(file)]
}

#[cfg(windows)]
fn candidates(dir: &Path, file: &str) -> Vec<PathBuf> {
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    exts.split(';')
        .filter(|e| !e.is_empty())
        .map(|e| dir.join(format!("{file}{}", e.to_ascii_lowercase())))
        .collect()
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Runs `program` for `call`, with this process's stdin, stdout and stderr,
/// and returns the exit code to leave with. On Unix the program replaces
/// this process, so this returns only if it could not be started.
pub fn run(program: &Path, call: &Call) -> i32 {
    let mut cmd = Command::new(program);
    cmd.args(&call.args);
    if let Some(profile) = &call.profile {
        cmd.env(PROFILE_ENV, profile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("fhir-pipes: could not run {}: {err}", program.display());
        126
    }
    #[cfg(not(unix))]
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("fhir-pipes: could not run {}: {err}", program.display());
            126
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_call_starts_at_the_first_command() {
        assert_eq!(
            call(&os(&["hello", "a", "--profile", "b"]), "hello"),
            Some(Call {
                name: "hello".into(),
                args: os(&["a", "--profile", "b"]),
                profile: None,
            })
        );
        assert_eq!(
            call(&os(&["--profile", "p.toml", "hello"]), "hello").map(|c| c.profile),
            Some(Some("p.toml".into()))
        );
        assert_eq!(
            call(&os(&["--profile=p.toml", "hello", "x"]), "hello").map(|c| (c.profile, c.args)),
            Some((Some("p.toml".into()), os(&["x"])))
        );
        // An unknown subcommand of a built-in command is not an external one.
        assert_eq!(call(&os(&["dhis2", "hello"]), "hello"), None);
        assert_eq!(call(&os(&["--verbose", "hello"]), "hello"), None);
        assert_eq!(call(&os(&["../x"]), "../x"), None);
    }

    #[test]
    fn the_folder_of_the_executable_comes_before_the_path() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) {
            "fhir-pipes-hi.exe"
        } else {
            "fhir-pipes-hi"
        };
        for dir in [a.path(), b.path()] {
            let p = dir.join(name);
            std::fs::write(&p, "").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let path = std::env::join_paths([b.path()]).unwrap();
        assert_eq!(
            find("hi", Some(a.path()), Some(&path)),
            Some(a.path().join(name))
        );
        assert_eq!(find("hi", None, Some(&path)), Some(b.path().join(name)));
        assert_eq!(find("other", Some(a.path()), Some(&path)), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_is_not_executable_is_not_a_command() {
        let a = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("fhir-pipes-hi"), "").unwrap();
        assert_eq!(find("hi", Some(a.path()), None), None);
    }
}
