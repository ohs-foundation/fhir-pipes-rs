//! External commands: `fhir-pipes NAME ARGS…` runs `fhir-pipes-NAME` from
//! the PATH when NAME is not a built-in command.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;

/// `fhir-pipes` with `dir` first on the PATH and no profile in the
/// environment.
fn fhir_pipes(dir: &Path) -> Command {
    let mut path = vec![dir.to_path_buf()];
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let mut c = Command::cargo_bin("fhir-pipes").unwrap();
    c.env("PATH", std::env::join_paths(path).unwrap())
        .env_remove("FHIR_PIPES_PROFILE");
    c
}

/// A program `fhir-pipes-hello` in `dir` that prints each argument on a
/// line, then `profile=$FHIR_PIPES_PROFILE`, and exits 3. The script is
/// written here, so its line ends do not depend on the checkout.
#[cfg(unix)]
fn write_hello(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fhir-pipes-hello");
    std::fs::write(
        &p,
        "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\"; done\necho \"profile=$FHIR_PIPES_PROFILE\"\nexit 3\n",
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The same as a Windows batch file. `%~1` is the argument without quotes;
/// the test passes two arguments.
#[cfg(windows)]
fn write_hello(dir: &Path) {
    std::fs::write(
        dir.join("fhir-pipes-hello.cmd"),
        "@echo off\r\necho %~1\r\necho %~2\r\necho profile=%FHIR_PIPES_PROFILE%\r\nexit /b 3\r\n",
    )
    .unwrap();
}

fn lines(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .map(|l| l.trim_end_matches('\r').trim_end().to_string())
        .collect()
}

#[test]
fn an_unknown_command_runs_the_program_on_the_path() {
    let dir = tempfile::tempdir().unwrap();
    write_hello(dir.path());
    let out = fhir_pipes(dir.path())
        .args(["hello", "a", "b"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    // The profile line is `profile=` with nothing set (Windows echoes the
    // unset variable's name).
    let got = lines(&out.stdout);
    assert_eq!(got[..2], ["a", "b"], "{got:?}");
    assert!(
        got[2] == "profile=" || got[2] == "profile=%FHIR_PIPES_PROFILE%",
        "{got:?}"
    );
}

#[test]
fn the_profile_given_before_the_command_reaches_the_program() {
    let dir = tempfile::tempdir().unwrap();
    write_hello(dir.path());
    let out = fhir_pipes(dir.path())
        .args(["--profile", "p.toml", "hello", "a", "b"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert_eq!(lines(&out.stdout), ["a", "b", "profile=p.toml"]);
}

#[test]
fn a_missing_program_is_an_unknown_command() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::cargo_bin("fhir-pipes")
        .unwrap()
        .env("PATH", OsString::from(dir.path()))
        .args(["nosuch", "a"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let e = String::from_utf8_lossy(&out.stderr);
    assert!(e.contains("unrecognized subcommand 'nosuch'"), "{e}");
    assert!(
        e.contains("fhir-pipes-nosuch was not found next to fhir-pipes or on the PATH"),
        "{e}"
    );
    // An unknown subcommand of a built-in command is only clap's error.
    let out = Command::cargo_bin("fhir-pipes")
        .unwrap()
        .args(["dhis2", "nosuch"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let e = String::from_utf8_lossy(&out.stderr);
    assert!(e.contains("unrecognized subcommand 'nosuch'"), "{e}");
    assert!(!e.contains("fhir-pipes-nosuch"), "{e}");
}
