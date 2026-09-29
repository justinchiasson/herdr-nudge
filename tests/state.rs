//! State files: atomic writes, and recovery from files we can't use.

mod support;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use herdr_nudge::classify::PaneKind;
use herdr_nudge::cli::JobId;
use herdr_nudge::event::AgentStatus;
use herdr_nudge::state::{
    AgentsCache, Job, Loaded, StateDir, VERSION, in_protected_folder, server_state_dirs,
    write_atomic,
};
use support::{Recorded, Replay, scratch_dir};

fn leftovers(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_missing_file_is_missing_not_an_error() {
    let state = StateDir::new(scratch_dir("a_missing_file_is_missing_not_an_error"));
    assert!(matches!(state.agents_cache().unwrap(), Loaded::Missing));
}

#[test]
fn a_write_leaves_no_temp_file_and_is_private() {
    let dir = scratch_dir("a_write_leaves_no_temp_file_and_is_private");
    let path = dir.join("f.json");
    write_atomic(&path, b"one").unwrap();
    write_atomic(&path, b"two").unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"two");
    assert_eq!(leftovers(&dir), ["f.json"]);
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "f.json mode");
}

#[test]
fn a_write_creates_missing_parent_directories() {
    let dir = scratch_dir("a_write_creates_missing_parent_directories");
    let path = dir.join("jobs/nested/f.json");
    write_atomic(&path, b"x").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"x");
}

/// Renaming a file over a non-empty directory fails, which makes the write
/// fail after its temp file already exists.
#[test]
fn a_failed_write_removes_its_temp_file() {
    let dir = scratch_dir("a_failed_write_removes_its_temp_file");
    let path = dir.join("f.json");
    write_atomic(&path, b"old").unwrap();

    let blocker = dir.join("blocker");
    fs::create_dir_all(blocker.join("inside")).unwrap();
    assert!(write_atomic(&blocker, b"new").is_err());

    assert_eq!(fs::read(&path).unwrap(), b"old");
    assert_eq!(
        leftovers(&dir),
        ["blocker", "f.json"],
        "temp file left behind"
    );
}

/// Readers running alongside a writer must only ever see whole files.
#[test]
fn a_reader_never_sees_a_half_written_file() {
    let dir = scratch_dir("a_reader_never_sees_a_half_written_file");
    let path = dir.join("f.json");
    let small = vec![b'a'; 10];
    let large = vec![b'b'; 1 << 20];
    write_atomic(&path, &small).unwrap();

    let writer = {
        let (path, small, large) = (path.clone(), small.clone(), large.clone());
        std::thread::spawn(move || {
            for i in 0..50 {
                write_atomic(&path, if i % 2 == 0 { &large } else { &small }).unwrap();
            }
        })
    };
    while !writer.is_finished() {
        let read = fs::read(&path).unwrap();
        assert!(read == small || read == large, "read {} bytes", read.len());
    }
    writer.join().unwrap();
}

#[test]
fn a_write_replaces_a_symlink_instead_of_following_it() {
    let dir = scratch_dir("a_write_replaces_a_symlink_instead_of_following_it");
    let target = dir.join("elsewhere.txt");
    fs::write(&target, b"untouched").unwrap();
    let path = dir.join("f.json");
    symlink(&target, &path).unwrap();

    write_atomic(&path, b"ours").unwrap();

    assert_eq!(fs::read(&target).unwrap(), b"untouched");
    assert!(
        !fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&path).unwrap(), b"ours");
}

fn assert_recovered(state: &StateDir, contents: &[u8], test: &str) {
    let path = state.agents_cache_path();
    fs::write(&path, contents).unwrap();

    let Loaded::Recovered(recovered) = state.agents_cache().unwrap() else {
        panic!("{test}: expected Recovered");
    };
    let aside = recovered.moved_to.expect("moved aside, not deleted");
    assert_eq!(
        fs::read(&aside).unwrap(),
        contents,
        "{test}: set-aside copy"
    );
    assert!(!path.exists(), "{test}: original still in place");

    // Recovery means the next save and load work normally.
    let cache = AgentsCache::new(["claude".to_owned()], 1);
    state.save_agents_cache(&cache).unwrap();
    assert!(matches!(state.agents_cache().unwrap(), Loaded::Found(c) if c == cache));
}

#[test]
fn a_corrupt_file_is_moved_aside_and_replaced() {
    for (name, contents) in [
        ("truncated", &b"{\"version\":1,\"agen"[..]),
        ("empty", b""),
        ("not_json", b"\x00\xff garbage"),
        (
            "wrong_shape",
            b"{\"version\":1,\"fetched_at_ms\":0,\"agents\":[1,2]}",
        ),
    ] {
        let state = StateDir::new(scratch_dir(&format!("corrupt_{name}")));
        assert_recovered(&state, contents, name);
    }
}

#[test]
fn a_file_from_another_version_is_moved_aside() {
    let state = StateDir::new(scratch_dir("a_file_from_another_version_is_moved_aside"));
    let newer = format!(
        "{{\"version\":{},\"fetched_at_ms\":0,\"agents\":[]}}",
        VERSION + 1
    );
    assert_recovered(&state, newer.as_bytes(), "newer version");
}

#[test]
fn a_symlinked_state_file_is_not_followed() {
    let dir = scratch_dir("a_symlinked_state_file_is_not_followed");
    let target = dir.join("elsewhere.json");
    fs::write(
        &target,
        b"{\"version\":1,\"fetched_at_ms\":0,\"agents\":[]}",
    )
    .unwrap();
    let state = StateDir::new(&dir);
    symlink(&target, state.agents_cache_path()).unwrap();

    let Loaded::Recovered(recovered) = state.agents_cache().unwrap() else {
        panic!("agents-cache.json symlink: expected Recovered");
    };
    assert_eq!(recovered.reason, "not a regular file");
    assert!(target.exists(), "the symlink's target was touched");
}

#[test]
fn a_recovered_file_reads_as_the_default() {
    let state = StateDir::new(scratch_dir("a_recovered_file_reads_as_the_default"));
    fs::write(state.agents_cache_path(), b"nope").unwrap();
    assert_eq!(
        state.agents_cache().unwrap().into_value(),
        AgentsCache::default()
    );
}

/// The click can read a state file while an event hook is reading the same
/// one. Both find it corrupt, only one can move it aside, and the loser
/// must still come back with something usable.
#[test]
fn two_readers_of_one_corrupt_file_both_recover() {
    let state = StateDir::new(scratch_dir("two_readers_of_one_corrupt_file"));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let state = state.clone();
            std::thread::spawn(move || {
                for _ in 0..50 {
                    fs::write(state.agents_cache_path(), b"not json").unwrap();
                    state.agents_cache().expect("read of a corrupt file failed");
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
}

#[test]
fn the_agents_cache_round_trips() {
    let state = StateDir::new(scratch_dir("the_agents_cache_round_trips"));
    let cache = AgentsCache::new(["claude", "codex"].map(String::from), 1_000);
    state.save_agents_cache(&cache).unwrap();

    match state.agents_cache().unwrap() {
        Loaded::Found(read) => {
            assert_eq!(read, cache);
            assert_eq!(read.labels().collect::<Vec<_>>(), ["claude", "codex"]);
        }
        other => panic!("agents-cache.json: expected Found, got {other:?}"),
    }
}

/// A job file name is a job id, and everything else in the directory is
/// something we put there ourselves: temp files and set-aside corrupt ones.
#[test]
fn listing_jobs_skips_names_that_are_not_job_ids() {
    let state = StateDir::new(scratch_dir("job_ids_skip_junk"));
    let id = JobId::parse("0123456789abcdef").unwrap();
    state.save_job(&job(&id)).unwrap();

    let jobs = state.jobs_dir();
    for name in [
        ".0123456789abcdef.json.999.0.tmp",
        "0123456789abcdef.json.corrupt-1700000000000",
        "not-a-job.json",
        "0123456789ABCDEF.json",
        "0123456789abcde.json",
        "0123456789abcdef.txt",
    ] {
        fs::write(jobs.join(name), b"{}").unwrap();
    }

    assert_eq!(
        state.job_ids().unwrap(),
        vec![id],
        "only the one real job should be listed"
    );
}

#[test]
fn listing_jobs_before_any_exist_is_empty_not_an_error() {
    let state = StateDir::new(scratch_dir("job_ids_empty"));
    assert_eq!(
        state.job_ids().unwrap(),
        Vec::<JobId>::new(),
        "no jobs directory yet"
    );
}

/// A job we cannot read must not stop a click or a sweep.
#[test]
fn a_corrupt_job_is_moved_aside() {
    let state = StateDir::new(scratch_dir("corrupt_job"));
    let id = JobId::parse("0123456789abcdef").unwrap();
    state.save_job(&job(&id)).unwrap();
    fs::write(state.job_path(&id), b"{ not json").unwrap();

    let Ok(Loaded::Recovered(recovered)) = state.job(&id) else {
        panic!("a corrupt job should be recovered, not returned");
    };
    assert_eq!(recovered.path, state.job_path(&id), "recovered path");
    assert!(
        matches!(state.job(&id), Ok(Loaded::Missing)),
        "after recovery the job should read as missing"
    );
}

/// Jobs written by an earlier build carry a `repeat_after_ms` that is no
/// longer read. They are still clickable for up to an hour after an upgrade,
/// so they have to keep loading.
#[test]
fn a_job_with_a_field_we_dropped_still_loads() {
    let state = StateDir::new(scratch_dir("job_extra_field"));
    let id = JobId::parse("0123456789abcdef").unwrap();
    state.save_job(&job(&id)).unwrap();
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(state.job_path(&id)).unwrap()).unwrap();
    json["repeat_after_ms"] = 5_000.into();
    fs::write(state.job_path(&id), json.to_string()).unwrap();

    let Ok(Loaded::Found(loaded)) = state.job(&id) else {
        panic!("a job with repeat_after_ms in it did not load");
    };
    assert_eq!(loaded, job(&id), "the job, minus repeat_after_ms");
}

#[test]
fn a_job_id_is_the_file_name() {
    let state = StateDir::new(Path::new("/state"));
    let id = JobId::parse("0123456789abcdef").unwrap();
    assert_eq!(
        state.job_path(&id),
        Path::new("/state/jobs/0123456789abcdef.json"),
        "job path"
    );
}

fn job(id: &JobId) -> Job {
    Job {
        version: VERSION,
        id: id.to_string(),
        pane_id: "w1:p1".to_owned(),
        workspace_id: "w1".to_owned(),
        agent_label: Some("claude".to_owned()),
        kind: PaneKind::Agent,
        status: AgentStatus::Blocked,
        group: "herdr-nudge-w1:p1".to_owned(),
        bundle_id: None,
        detect_at_click: false,
        released_by_herdr: false,
        socket_path: std::path::PathBuf::from("/tmp/herdr.sock"),
        notifier_path: std::path::PathBuf::from("/plugin/notifier"),
        created_at_ms: 1_000,
        expires_at_ms: 9_000,
    }
}

/// Three servers: `nudgexdg` started with `XDG_STATE_HOME`, `nudgeplain`
/// without, and the default session's, whose environment wasn't captured
/// (it's the user's whole environment), so `ps` on it has no recording.
#[test]
fn each_server_state_directory_comes_from_its_own_environment() {
    let runner = Replay::new([
        Recorded::pgrep_herdr("pgrep-herdr-three-servers"),
        Recorded::sys("ps-env-server-xdg"),
        Recorded::sys("ps-env-server-plain"),
    ]);
    let mut notes = Vec::new();
    let dirs = server_state_dirs(&runner, None, &mut notes);

    let found: Vec<(u32, String, bool)> = dirs
        .iter()
        .map(|d| (d.pid, d.dir.root.display().to_string(), d.protected))
        .collect();
    assert_eq!(
        found,
        vec![
            (
                15337,
                "/private/tmp/claude-501/-Users-dev-Developer-herdr-nudge/00000000-0000-4000-8000-000000000004/scratchpad/xdg-state/herdr/plugins/herdr-nudge".to_owned(),
                false
            ),
            (
                16451,
                "/Users/dev/.local/state/herdr/plugins/herdr-nudge".to_owned(),
                false
            ),
        ],
        "sys/ps-env-server-xdg and sys/ps-env-server-plain"
    );
    assert_eq!(
        notes,
        vec!["could not read herdr server 85985's environment".to_owned()]
    );
}

#[test]
fn protected_folders_are_matched_by_whole_path_components() {
    let home = Path::new("/Users/dev");
    assert!(in_protected_folder(
        Path::new("/Users/dev/Documents/state"),
        home
    ));
    assert!(in_protected_folder(Path::new("/Users/dev/Desktop"), home));
    assert!(!in_protected_folder(
        Path::new("/Users/dev/Documents-old/state"),
        home
    ));
    assert!(!in_protected_folder(
        Path::new("/Users/dev/.local/state"),
        home
    ));
}

#[test]
fn protected_folders_ignore_case_and_refuse_dot_dot() {
    let home = Path::new("/Users/dev");
    assert!(in_protected_folder(
        Path::new("/Users/dev/documents/state"),
        home
    ));
    assert!(in_protected_folder(Path::new("/users/DEV/DOWNLOADS"), home));
    assert!(in_protected_folder(
        Path::new("/Users/dev/x/../Documents"),
        home
    ));
}

/// `nudgedocs` was started with `HOME=/tmp/hnd/a` and its state in that
/// home's Documents; `nudgenohome` with no `HOME` at all and its state in
/// `/tmp/hnd/b/Documents`. Neither is the user's real home.
#[test]
fn a_server_state_directory_in_documents_is_protected_by_either_home() {
    let runner = || {
        Replay::new([
            Recorded::pgrep_herdr("pgrep-herdr-documents-servers"),
            Recorded::sys("ps-env-server-documents"),
            Recorded::sys("ps-env-server-no-home"),
        ])
    };
    let protected = |home: Option<&Path>| -> Vec<(u32, bool)> {
        server_state_dirs(&runner(), home, &mut Vec::new())
            .iter()
            .map(|d| (d.pid, d.protected))
            .collect()
    };

    assert_eq!(
        protected(None),
        vec![(24641, true), (24642, false)],
        "only the server's own HOME to go on"
    );
    assert_eq!(
        protected(Some(Path::new("/tmp/hnd/b"))),
        vec![(24641, true), (24642, true)],
        "the caller's HOME catches the server that has none"
    );
}
