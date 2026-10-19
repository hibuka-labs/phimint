//! Tests for the browser launch port (1C row 20's automation half).
//!
//! Everything here is pure: the four platform argv forms, the WSL fallback
//! order, the loopback-URL guard, and the headless fallback. Actually opening
//! a browser needs a real machine and stays on the human checklist.

use super::*;

const URL: &str = "http://127.0.0.1:32123";

#[test]
fn macos_launches_open_without_stealing_focus() {
    assert_eq!(
        launch_commands(Platform::MacOs, URL),
        vec![CommandSpec {
            program: "open".into(),
            args: vec!["-g".into(), URL.into()],
        }]
    );
}

#[test]
fn linux_launches_xdg_open() {
    assert_eq!(
        launch_commands(Platform::Linux, URL),
        vec![CommandSpec {
            program: "xdg-open".into(),
            args: vec![URL.into()],
        }]
    );
}

#[test]
fn windows_launches_start_with_an_empty_title() {
    // `start` takes the first quoted argument as the window title — the `""`
    // keeps the URL from being swallowed as one.
    assert_eq!(
        launch_commands(Platform::Windows, URL),
        vec![CommandSpec {
            program: "cmd".into(),
            args: vec!["/c".into(), "start".into(), String::new(), URL.into()],
        }]
    );
}

#[test]
fn wsl_tries_wslview_then_the_windows_opener() {
    assert_eq!(
        launch_commands(Platform::Wsl, URL),
        vec![
            CommandSpec {
                program: "wslview".into(),
                args: vec![URL.into()],
            },
            CommandSpec {
                program: "cmd.exe".into(),
                args: vec!["/c".into(), "start".into(), String::new(), URL.into()],
            },
        ]
    );
}

#[test]
fn platform_detection_maps_the_host_facts() {
    assert_eq!(detect_platform("macos", false), Platform::MacOs);
    assert_eq!(detect_platform("windows", false), Platform::Windows);
    assert_eq!(detect_platform("linux", false), Platform::Linux);
    // WSL is linux by kernel string, windows by opener family.
    assert_eq!(detect_platform("linux", true), Platform::Wsl);
}

#[test]
fn only_loopback_http_rides_the_launch_port() {
    assert!(is_canvas_url(URL));
    assert!(is_canvas_url("http://localhost:8080"));
    assert!(!is_canvas_url("https://example.com"));
    assert!(!is_canvas_url("file:///etc/passwd"));
    assert!(!is_canvas_url("http://192.168.1.10:80"));
}

#[test]
fn non_canvas_urls_never_reach_the_opener() {
    let mut spawned = Vec::new();
    let outcome = launch_with(Platform::MacOs, "https://example.com", |spec| {
        spawned.push(spec.clone());
        true
    });
    assert_eq!(outcome, LaunchOutcome::ShowUrl, "refused ⇒ headless path");
    assert!(spawned.is_empty(), "no opener may see a foreign URL");
}

#[test]
fn wsl_falls_back_to_the_windows_opener_when_wslview_is_missing() {
    let mut tried = Vec::new();
    let outcome = launch_with(Platform::Wsl, URL, |spec| {
        tried.push(spec.program.clone());
        spec.program != "wslview"
    });
    assert_eq!(tried, vec!["wslview", "cmd.exe"], "best candidate first");
    assert_eq!(outcome, LaunchOutcome::Spawned);
}

#[test]
fn headless_is_the_fallback_when_no_opener_spawns() {
    let outcome = launch_with(Platform::Linux, URL, |_| false);
    assert_eq!(outcome, LaunchOutcome::ShowUrl, "print the URL, carry on");
}
