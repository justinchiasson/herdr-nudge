//! The context JSON and the `HERDR_*` environment, as captured.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use herdr_nudge::context::{Context, Env, plugin_root_above};
use support::Fixture;

#[test]
fn every_fixture_context_parses_and_names_its_workspace() {
    for fixture in Fixture::all() {
        let context = fixture.context();
        assert_eq!(
            Some(context.workspace_id.as_str()),
            fixture.env.get("HERDR_WORKSPACE_ID").map(String::as_str),
            "{}: workspace_id",
            fixture.name
        );
        assert_eq!(
            context.tab_id.as_deref(),
            fixture.env.get("HERDR_TAB_ID").map(String::as_str),
            "{}: tab_id",
            fixture.name
        );
    }
}

/// A closing pane gets a much smaller context: four fields, no `tab_id` and
/// no `workspace_label`.
#[test]
fn a_context_missing_everything_optional_still_parses() {
    let fixture = Fixture::load("lifecycle/pane-closed");
    for absent in ["workspace_label", "tab_id"] {
        assert!(
            !fixture.context_json().contains(absent),
            "fixture no longer covers a context without {absent}"
        );
    }

    let context = fixture.context();
    assert_eq!(context.workspace_id, "w1");
    assert_eq!(context.workspace_label, None);
    assert_eq!(context.tab_id, None);
    // Falls back to the id rather than going blank.
    assert_eq!(context.workspace_display(), "w1");
}

#[test]
fn a_labelled_workspace_displays_its_label() {
    let context = Fixture::load("agent/blocked").context();
    assert_eq!(context.workspace_label.as_deref(), Some("herdr-nudge"));
    assert_eq!(context.workspace_display(), "herdr-nudge");
}

#[test]
fn every_fixture_environment_resolves_to_paths() {
    for fixture in Fixture::all() {
        let env = fixture.herdr_env();
        // The 0.9.0 close captures come from a throwaway server with its own
        // home, so its socket is there; `herdr` was the downloaded release.
        if fixture.source.contains("herdr-0.9.0-closes") {
            assert_eq!(
                env.socket_path,
                PathBuf::from("/tmp/r1h/.config/herdr/herdr.sock"),
                "{}",
                fixture.name
            );
            assert!(
                env.herdr_bin.ends_with("herdr-0.9.0"),
                "{}: herdr_bin {}",
                fixture.name,
                env.herdr_bin.display()
            );
        } else {
            assert_eq!(
                env.herdr_bin,
                PathBuf::from("/Users/dev/.local/bin/herdr"),
                "{}",
                fixture.name
            );
            // The 0.9.2 clear capture comes from a throwaway server too,
            // with its own home and a named session.
            let socket = if fixture.source.contains("herdr-0.9.2-clear") {
                "/tmp/hxclear/.config/herdr/sessions/hnx/herdr.sock"
            } else {
                "/Users/dev/.config/herdr/herdr.sock"
            };
            assert_eq!(env.socket_path, PathBuf::from(socket), "{}", fixture.name);
        }
        assert!(
            env.state_dir.is_absolute() && env.config_dir.is_absolute(),
            "{}: relative paths",
            fixture.name
        );
        assert!(
            env.plugin_root.is_absolute(),
            "{}: plugin_root",
            fixture.name
        );
    }
}

#[test]
fn a_missing_variable_is_named_rather_than_guessed() {
    let mut env: BTreeMap<String, String> = Fixture::load("agent/blocked").env.clone();
    env.remove("HERDR_SOCKET_PATH");
    env.insert("HERDR_PLUGIN_STATE_DIR".into(), String::new());

    let err = Env::from_map(&env).expect_err("an incomplete environment must not be papered over");
    let message = err.to_string();
    assert!(message.contains("HERDR_SOCKET_PATH"), "{message}");
    assert!(message.contains("HERDR_PLUGIN_STATE_DIR"), "{message}");
    assert!(!message.contains("HERDR_BIN_PATH"), "{message}");
}

/// The context also carries `focused_pane_id` and `invocation_source`, which
/// look useful and aren't (see the note in `src/context.rs`). `Context`
/// doesn't read either one, and this keeps it that way: in `src/` they may
/// only appear in comments.
#[test]
fn the_misleading_context_fields_are_not_read_anywhere() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();

    for entry in std::fs::read_dir(&src).expect("reading src/") {
        let path = entry.expect("directory entry").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("reading a source file");
        for (number, line) in text.lines().enumerate() {
            let is_comment = line.trim_start().starts_with("//");
            for field in ["focused_pane_id", "invocation_source"] {
                if line.contains(field) && !is_comment {
                    offenders.push(format!(
                        "{}:{}: {field}",
                        path.file_name().unwrap().to_string_lossy(),
                        number + 1
                    ));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these fields must never be read as code:\n  {}",
        offenders.join("\n  ")
    );
}

/// Each fixture names the raw log line it came from. That link is the only
/// thing tying a fixture back to the capture, so check it still holds.
#[test]
fn every_fixture_points_at_its_line_in_a_raw_log() {
    for fixture in Fixture::all() {
        let (file, line) = fixture
            .source
            .rsplit_once(':')
            .unwrap_or_else(|| panic!("{}: malformed source {:?}", fixture.name, fixture.source));
        let line: usize = line
            .parse()
            .unwrap_or_else(|e| panic!("{}: {e} in source {:?}", fixture.name, fixture.source));
        let log = file
            .rsplit('/')
            .next()
            .unwrap_or_else(|| panic!("{}: malformed source", fixture.name));

        let record = support::raw_log(log)
            .into_iter()
            .find(|r| r.line == line)
            .unwrap_or_else(|| panic!("{}: no line {line} in {log}", fixture.name));

        assert_eq!(record.kind, fixture.event, "{}: event name", fixture.name);
        assert_eq!(record.rest, fixture.event_json, "{}: payload", fixture.name);
    }
}

#[test]
fn a_context_with_only_a_workspace_is_enough() {
    // Nothing real is this small, but workspace_id is the only field we can
    // count on.
    let context = Context::parse(r#"{"workspace_id":"w9"}"#).expect("minimal context");
    assert_eq!(context.workspace_display(), "w9");
}

/// The binary sits in `target/release/` while developing and in `bin/` once
/// installed; either way the manifest is further up.
#[test]
fn the_plugin_root_is_the_nearest_directory_with_a_manifest() {
    let root = support::scratch_dir("plugin_root_above");
    fs::write(root.join("herdr-plugin.toml"), "").unwrap();
    let dev = root.join("target/release");
    fs::create_dir_all(&dev).unwrap();
    // A manifest named like the binary doesn't count: only a directory's.
    fs::create_dir_all(dev.join("herdr-plugin.toml")).unwrap();

    for binary in [dev.join("herdr-nudge"), root.join("bin/herdr-nudge")] {
        assert_eq!(
            plugin_root_above(&binary).as_deref(),
            Some(root.as_path()),
            "{}",
            binary.display()
        );
    }
    assert_eq!(plugin_root_above(Path::new("/usr/bin/herdr-nudge")), None);
}
