//! End-to-end test against a local sshd (see scripts/test-server.sh).
//! Run with: cargo test -p linuxscp --test sftp_roundtrip -- --ignored

use linuxscp::types::{Backend, ConnectSpec, Event, TransferRequest, TransferState};
use linuxscp::{fsops, sessions, ssh, transfers};

const TEST_DIR: &str = "/tmp/linuxscp-test";

fn test_spec() -> ConnectSpec {
    let mut spec = ConnectSpec::new("testbox");
    spec.extra_ssh_args = vec![
        "-F".into(),
        format!("{TEST_DIR}/home/.ssh/config"),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
    ];
    spec
}

/// Consume events; auto-confirm prompts; return terminal transfer state.
async fn wait_for_transfer(
    events_rx: &async_channel::Receiver<Event>,
) -> (TransferState, Option<String>) {
    loop {
        match events_rx.recv().await.expect("event stream ended") {
            Event::Prompt(prompt) => {
                let _ = prompt.reply.send(Some("yes".into()));
            }
            Event::Conflict(conflict) => {
                let _ = conflict
                    .reply
                    .send(linuxscp::types::ConflictDecision::Overwrite);
            }
            Event::TransferUpdate(snap) if snap.state.is_terminal() => {
                return (snap.state, snap.error);
            }
            _ => {}
        }
    }
}

#[test]
#[ignore = "requires the local test sshd from scripts/test-server.sh"]
fn sftp_roundtrip_with_resume() {
    let rt = linuxscp::runtime::runtime();
    rt.block_on(async {
        let (events_tx, events_rx) = async_channel::unbounded::<Event>();

        // Auto-answer any host-key confirmation prompt during connect.
        let answerer = {
            let events_rx = events_rx.clone();
            tokio::spawn(async move {
                while let Ok(event) = events_rx.recv().await {
                    if let Event::Prompt(prompt) = event {
                        let _ = prompt.reply.send(Some("yes".into()));
                    }
                }
            })
        };

        let conn = ssh::connect::connect(1, test_spec(), events_tx.clone())
            .await
            .expect("connect failed");
        answerer.abort();
        sessions::register(
            1,
            sessions::SessionHandle::new(conn.sftp.clone(), conn.cancel.clone(), "testbox".into()),
        );
        let remote = Backend::Remote(1);

        // Listing works and sees the home dir.
        let home = conn.initial_dir.clone();
        let entries = fsops::list_dir(remote, &home).await.expect("list failed");
        assert!(!entries.is_empty(), "home dir listing came back empty");

        // Prepare a 4 MiB source file with a recognizable pattern.
        let src_dir = format!("{TEST_DIR}/xfer-src");
        let dst_dir = format!("{TEST_DIR}/xfer-dst");
        std::fs::remove_dir_all(&src_dir).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
        std::fs::remove_dir_all(format!("{TEST_DIR}/xfer-down")).ok();
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();
        let payload: Vec<u8> = (0..4 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(format!("{src_dir}/big.bin"), &payload).unwrap();

        // Upload: local -> remote (into dst_dir, which the sshd sees locally
        // since it is our own machine).
        let src_entry = fsops::stat(Backend::Local, &format!("{src_dir}/big.bin"))
            .await
            .unwrap();
        transfers::start(
            TransferRequest {
                src_backend: Backend::Local,
                dst_backend: remote,
                items: vec![src_entry.clone()],
                dst_dir: dst_dir.clone(),
                move_src: false,
                overwrite: false,
            },
            events_tx.clone(),
        );
        let (state, error) = wait_for_transfer(&events_rx).await;
        assert_eq!(state, TransferState::Done, "upload failed: {error:?}");
        let uploaded = std::fs::read(format!("{dst_dir}/big.bin")).unwrap();
        assert_eq!(uploaded, payload, "uploaded content differs");

        // Resume: truncate a copy into a .filepart and re-download; the
        // engine must continue from the partial offset and produce an
        // identical file.
        let down_dir = format!("{TEST_DIR}/xfer-down");
        std::fs::create_dir_all(&down_dir).unwrap();
        std::fs::write(
            format!("{down_dir}/big.bin{}", transfers::PART_SUFFIX),
            &payload[..1024 * 1024],
        )
        .unwrap();
        let remote_entry = fsops::stat(remote, &format!("{dst_dir}/big.bin"))
            .await
            .unwrap();
        transfers::start(
            TransferRequest {
                src_backend: remote,
                dst_backend: Backend::Local,
                items: vec![remote_entry],
                dst_dir: down_dir.clone(),
                move_src: false,
                overwrite: false,
            },
            events_tx.clone(),
        );
        let (state, error) = wait_for_transfer(&events_rx).await;
        assert_eq!(state, TransferState::Done, "download failed: {error:?}");
        let downloaded = std::fs::read(format!("{down_dir}/big.bin")).unwrap();
        assert_eq!(downloaded, payload, "resumed download corrupted the file");
        assert!(
            !std::path::Path::new(&format!("{down_dir}/big.bin{}", transfers::PART_SUFFIX))
                .exists(),
            ".filepart should be renamed away"
        );

        // Remote file management: mkdir, rename, chmod, delete.
        let tree = format!("{dst_dir}/nested/deep");
        fsops::mkdir_all(remote, &tree).await.unwrap();
        assert!(fsops::exists(remote, &tree).await);
        fsops::rename(remote, &tree, &format!("{dst_dir}/nested/renamed"))
            .await
            .unwrap();
        fsops::chmod(remote, &format!("{dst_dir}/big.bin"), 0o600)
            .await
            .unwrap();
        let meta = fsops::stat(remote, &format!("{dst_dir}/big.bin"))
            .await
            .unwrap();
        assert_eq!(meta.mode, Some(0o600));
        let nested = fsops::stat(remote, &format!("{dst_dir}/nested"))
            .await
            .unwrap();
        fsops::delete(remote, &nested).await.unwrap();
        assert!(!fsops::exists(remote, &format!("{dst_dir}/nested")).await);

        // Symlink creation (OpenSSH swaps SYMLINK's wire arguments; this
        // asserts we compensate correctly).
        let link = format!("{dst_dir}/big-link");
        fsops::symlink(remote, &link, "big.bin").await.unwrap();
        let link_meta = std::fs::symlink_metadata(&link).expect("symlink missing");
        assert!(link_meta.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(&link).unwrap().to_string_lossy(),
            "big.bin"
        );
        let listed = fsops::list_dir(remote, &dst_dir).await.unwrap();
        let link_entry = listed.iter().find(|e| e.name == "big-link").unwrap();
        assert!(link_entry.is_symlink);
        assert_eq!(link_entry.link_target.as_deref(), Some("big.bin"));

        // Remote listings resolve numeric uids to names via /etc/passwd.
        let me = std::process::Command::new("id")
            .arg("-un")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        if let Some(me) = me.filter(|n| !n.is_empty()) {
            let big = listed.iter().find(|e| e.name == "big.bin").unwrap();
            assert_eq!(
                big.owner.as_deref(),
                Some(me.as_str()),
                "owner should be a user name, not a uid"
            );
        }

        // Recursive disk usage sees the uploaded file.
        let usage = fsops::disk_usage(remote, &listed).await.unwrap();
        assert!(usage.files >= 2, "expected at least big.bin and the link");
        assert!(usage.bytes >= payload.len() as u64);

        // Properties plumbing: resolve our own name to ids, then apply a
        // recursive mode + owner change to a small tree.
        let me = String::from_utf8_lossy(
            &std::process::Command::new("id")
                .arg("-un")
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        let my_group = String::from_utf8_lossy(
            &std::process::Command::new("id")
                .arg("-gn")
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        let (uid, gid) = fsops::resolve_ids(remote, Some(me.clone()), Some(my_group.clone()))
            .await
            .expect("resolving own user/group failed");
        assert!(uid.is_some() && gid.is_some());

        let attr_root = format!("{dst_dir}/attrs/inner");
        fsops::mkdir_all(remote, &attr_root).await.unwrap();
        std::fs::write(format!("{attr_root}/leaf.txt"), b"x").unwrap();
        let root_entry = fsops::stat(remote, &format!("{dst_dir}/attrs"))
            .await
            .unwrap();
        let changes = fsops::AttrChanges {
            mode: Some(0o640),
            uid,
            gid,
            add_x_dirs: true,
        };
        let changed = fsops::apply_attrs(remote, &[root_entry], &changes, true, None)
            .await
            .expect("apply_attrs failed");
        assert_eq!(changed, 3, "attrs + inner + leaf.txt");
        // Directories got X added where R applies (0o640 -> 0o750);
        // files keep the plain mode.
        let dir_meta = fsops::stat(remote, &attr_root).await.unwrap();
        assert_eq!(dir_meta.mode, Some(0o750));
        let leaf_meta = fsops::stat(remote, &format!("{attr_root}/leaf.txt"))
            .await
            .unwrap();
        assert_eq!(leaf_meta.mode, Some(0o640));
        assert_eq!(leaf_meta.owner.as_deref(), Some(me.as_str()));

        sessions::close(1);
    });
}

/// Downloading a deep remote tree must copy files *while* still scanning:
/// the count is computed in parallel, not fully up front. Asserts that the
/// first file finishes before the scan has discovered them all.
#[test]
#[ignore = "requires the local test sshd from scripts/test-server.sh"]
fn remote_scan_and_copy_overlap() {
    let rt = linuxscp::runtime::runtime();
    rt.block_on(async {
        let (events_tx, events_rx) = async_channel::unbounded::<Event>();
        let answerer = {
            let events_rx = events_rx.clone();
            tokio::spawn(async move {
                while let Ok(event) = events_rx.recv().await {
                    if let Event::Prompt(prompt) = event {
                        let _ = prompt.reply.send(Some("yes".into()));
                    }
                }
            })
        };
        let conn = ssh::connect::connect(4, test_spec(), events_tx.clone())
            .await
            .expect("connect failed");
        answerer.abort();
        sessions::register(
            4,
            sessions::SessionHandle::new(conn.sftp.clone(), conn.cancel.clone(), "testbox".into()),
        );
        let remote = Backend::Remote(4);

        // A broad tree of many small files: discovering them all takes many
        // SFTP round-trips, so scanning is much slower than copying one file.
        let src = format!("{TEST_DIR}/overlap-src");
        let dst = format!("{TEST_DIR}/overlap-dst");
        std::fs::remove_dir_all(&src).ok();
        std::fs::remove_dir_all(&dst).ok();
        let mut expected = 0u32;
        for d in 0..25 {
            let dir = format!("{src}/dir{d:02}");
            std::fs::create_dir_all(&dir).unwrap();
            for f in 0..20 {
                std::fs::write(format!("{dir}/f{f:02}.bin"), vec![b'x'; 16 * 1024]).unwrap();
                expected += 1;
            }
        }
        std::fs::create_dir_all(&dst).unwrap();

        let src_entry = fsops::stat(remote, &src).await.unwrap();
        transfers::start(
            TransferRequest {
                src_backend: remote,
                dst_backend: Backend::Local,
                items: vec![src_entry],
                dst_dir: dst.clone(),
                move_src: false,
                overwrite: false,
            },
            events_tx.clone(),
        );

        // Record the file total known at the moment the first file finished,
        // and whether we ever saw copying happen while still scanning.
        let mut total_at_first_copy: Option<u32> = None;
        let mut copied_while_scanning = false;
        let final_snap = loop {
            if let Event::TransferUpdate(snap) = events_rx.recv().await.expect("event stream ended")
            {
                if snap.files_done >= 1 {
                    if total_at_first_copy.is_none() {
                        total_at_first_copy = Some(snap.files_total);
                    }
                    if snap.scanning {
                        copied_while_scanning = true;
                    }
                }
                if snap.state.is_terminal() {
                    break snap;
                }
            }
        };
        assert_eq!(final_snap.state, TransferState::Done);

        // Parallelism: when the first file was already done, the scanner had
        // not yet discovered the whole tree.
        let seen = total_at_first_copy.expect("never observed a copied file");
        assert!(
            seen < expected,
            "expected scanning to still be in progress at first copy \
             (known={seen}, final={expected}) — scan and copy did not overlap"
        );

        // The `scanning` flag drives the smart progress bar/ETA: it must be
        // set while files are copied mid-scan, and cleared by the end (when
        // the total is final).
        assert!(
            copied_while_scanning,
            "scanning flag was never true while copying — the UI could never \
             show the indeterminate/counting state"
        );
        assert!(
            !final_snap.scanning,
            "scanning flag must be cleared once the tree is fully counted"
        );
        assert_eq!(final_snap.files_total, expected, "final total is exact");

        // And the whole tree arrives intact.
        let copied = std::fs::read_dir(format!("{dst}/overlap-src"))
            .unwrap()
            .filter_map(|e| e.ok())
            .flat_map(|d| std::fs::read_dir(d.path()).unwrap())
            .filter(|e| e.as_ref().unwrap().path().is_file())
            .count();
        assert_eq!(copied as u32, expected, "every file downloaded");

        std::fs::remove_dir_all(&src).ok();
        std::fs::remove_dir_all(&dst).ok();
        sessions::close(4);
    });
}

/// Elevated connect through the pty flow, against the fake `su` shim the
/// test sshd puts on PATH (password: secret123).
#[test]
#[ignore = "requires the local test sshd from scripts/test-server.sh"]
fn su_elevation_pty_flow() {
    let rt = linuxscp::runtime::runtime();
    rt.block_on(async {
        let (events_tx, events_rx) = async_channel::unbounded::<Event>();

        // Answer host-key prompts with yes and password prompts with the
        // fake su's password.
        tokio::spawn(async move {
            while let Ok(event) = events_rx.recv().await {
                if let Event::Prompt(prompt) = event {
                    let answer = if prompt.prompt.contains("password on") {
                        "secret123"
                    } else {
                        "yes"
                    };
                    let _ = prompt.reply.send(Some(answer.into()));
                }
            }
        });

        let mut spec = test_spec();
        spec.elevation = linuxscp::types::Elevation::Su;
        let conn = ssh::connect::connect(2, spec, events_tx.clone())
            .await
            .expect("elevated connect failed");
        sessions::register(
            2,
            sessions::SessionHandle::new(conn.sftp.clone(), conn.cancel.clone(), "testbox".into()),
        );

        // The elevated SFTP session must be fully functional.
        let remote = Backend::Remote(2);
        let dir = format!("{TEST_DIR}/su-elevated");
        fsops::mkdir_all(remote, &dir).await.unwrap();
        let listing = fsops::list_dir(remote, TEST_DIR).await.unwrap();
        assert!(listing.iter().any(|e| e.name == "su-elevated"));

        // Binary integrity through the raw pty: round-trip all byte values.
        let payload: Vec<u8> = (0..=255u8).cycle().take(128 * 1024).collect();
        std::fs::write(format!("{TEST_DIR}/pty-src.bin"), &payload).unwrap();
        let entry = fsops::stat(Backend::Local, &format!("{TEST_DIR}/pty-src.bin"))
            .await
            .unwrap();
        transfers::start(
            TransferRequest {
                src_backend: Backend::Local,
                dst_backend: remote,
                items: vec![entry],
                dst_dir: dir.clone(),
                move_src: false,
                overwrite: false,
            },
            events_tx.clone(),
        );
        let (events_tx2, events_rx2) = async_channel::unbounded::<Event>();
        let _ = events_tx2; // silence unused when not prompting
        drop(events_rx2);
        // Wait on the original channel is consumed by the answerer task, so
        // poll the file instead.
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Ok(done) = std::fs::read(format!("{dir}/pty-src.bin")) {
                if done == payload {
                    break;
                }
            }
        }
        let uploaded = std::fs::read(format!("{dir}/pty-src.bin")).expect("upload missing");
        assert_eq!(uploaded, payload, "pty mangled binary data");

        sessions::close(2);
    });
}

/// Wrong su password must fail cleanly, not hang.
#[test]
#[ignore = "requires the local test sshd from scripts/test-server.sh"]
fn su_elevation_wrong_password() {
    let rt = linuxscp::runtime::runtime();
    rt.block_on(async {
        let (events_tx, events_rx) = async_channel::unbounded::<Event>();
        tokio::spawn(async move {
            while let Ok(event) = events_rx.recv().await {
                if let Event::Prompt(prompt) = event {
                    let answer = if prompt.prompt.contains("password on") {
                        "wrong"
                    } else {
                        "yes"
                    };
                    let _ = prompt.reply.send(Some(answer.into()));
                }
            }
        });

        let mut spec = test_spec();
        spec.elevation = linuxscp::types::Elevation::Su;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            ssh::connect::connect(3, spec, events_tx),
        )
        .await
        .expect("connect hung on wrong password");
        assert!(result.is_err(), "connect should fail with wrong password");
    });
}
