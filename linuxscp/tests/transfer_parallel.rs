//! Local→local transfer tests for the concurrent scan+copy engine: a nested
//! tree (with an empty directory) must copy correctly, the discovered totals
//! must reach the true count, moves must remove the source, and the
//! pre-answered overwrite policy must replace existing destinations without
//! ever raising a conflict.

use linuxscp::types::{Backend, Event, TransferRequest, TransferSnapshot, TransferState};
use linuxscp::{fsops, transfers};

/// Drain events to completion, returning the terminal snapshot (which
/// carries the final file/byte totals).
async fn drive(rx: &async_channel::Receiver<Event>) -> (TransferState, TransferSnapshot) {
    loop {
        // No conflicts or prompts are expected against a fresh dest.
        if let Event::TransferUpdate(snap) = rx.recv().await.expect("event stream ended") {
            if snap.state.is_terminal() {
                return (snap.state, snap);
            }
        }
    }
}

fn build_tree(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("tree/sub/deep")).unwrap();
    std::fs::create_dir_all(root.join("tree/empty")).unwrap();
    std::fs::write(root.join("tree/a.txt"), b"alpha").unwrap(); // 5
    std::fs::write(root.join("tree/sub/b.txt"), b"bravo!!").unwrap(); // 7
    std::fs::write(root.join("tree/sub/deep/c.txt"), vec![7u8; 5000]).unwrap(); // 5000
}

#[test]
fn nested_tree_copies_and_counts() {
    let rt = linuxscp::runtime::runtime();
    rt.block_on(async {
        let base = std::env::temp_dir().join("linuxscp-parallel-copy");
        std::fs::remove_dir_all(&base).ok();
        let src = base.join("src");
        let dst = base.join("dst");
        build_tree(&src);
        std::fs::create_dir_all(&dst).unwrap();

        let (tx, rx) = async_channel::unbounded::<Event>();
        let entry = fsops::stat(Backend::Local, &src.join("tree").to_string_lossy())
            .await
            .unwrap();
        transfers::start(
            TransferRequest {
                src_backend: Backend::Local,
                dst_backend: Backend::Local,
                items: vec![entry],
                dst_dir: dst.to_string_lossy().into_owned(),
                move_src: false,
                overwrite: false,
            },
            tx,
        );

        let (state, snap) = drive(&rx).await;
        assert_eq!(state, TransferState::Done);

        // The scanner's totals must have reached the true tree size.
        assert_eq!(snap.files_total, 3, "all files discovered");
        assert_eq!(snap.files_done, 3, "all files copied");
        assert_eq!(snap.total_bytes, 5 + 7 + 5000, "byte total counted");

        // Contents copied faithfully, at every depth.
        assert_eq!(std::fs::read(dst.join("tree/a.txt")).unwrap(), b"alpha");
        assert_eq!(
            std::fs::read(dst.join("tree/sub/b.txt")).unwrap(),
            b"bravo!!"
        );
        assert_eq!(
            std::fs::read(dst.join("tree/sub/deep/c.txt"))
                .unwrap()
                .len(),
            5000
        );
        // Empty directories are mirrored even though they hold no files.
        assert!(dst.join("tree/empty").is_dir(), "empty dir preserved");

        // Source untouched on a copy.
        assert!(src.join("tree/a.txt").exists());
        std::fs::remove_dir_all(&base).ok();
    });
}

#[test]
fn move_removes_source_tree() {
    let rt = linuxscp::runtime::runtime();
    rt.block_on(async {
        let base = std::env::temp_dir().join("linuxscp-parallel-move");
        std::fs::remove_dir_all(&base).ok();
        let src = base.join("src");
        let dst = base.join("dst");
        build_tree(&src);
        std::fs::create_dir_all(&dst).unwrap();

        let (tx, rx) = async_channel::unbounded::<Event>();
        let entry = fsops::stat(Backend::Local, &src.join("tree").to_string_lossy())
            .await
            .unwrap();
        transfers::start(
            TransferRequest {
                src_backend: Backend::Local,
                dst_backend: Backend::Local,
                items: vec![entry],
                dst_dir: dst.to_string_lossy().into_owned(),
                move_src: true,
                overwrite: false,
            },
            tx,
        );

        let (state, _snap) = drive(&rx).await;
        assert_eq!(state, TransferState::Done);

        // Everything arrived…
        assert_eq!(std::fs::read(dst.join("tree/a.txt")).unwrap(), b"alpha");
        assert_eq!(
            std::fs::read(dst.join("tree/sub/deep/c.txt"))
                .unwrap()
                .len(),
            5000
        );
        // …and the source tree is gone.
        assert!(!src.join("tree").exists(), "move removed the source");
        std::fs::remove_dir_all(&base).ok();
    });
}

/// `overwrite: true` (edit re-uploads) must replace an existing destination
/// silently: no conflict event, and the new contents land.
#[test]
fn overwrite_replaces_without_conflict() {
    let rt = linuxscp::runtime::runtime();
    rt.block_on(async {
        let base = std::env::temp_dir().join("linuxscp-parallel-overwrite");
        std::fs::remove_dir_all(&base).ok();
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("conf.txt"), b"new contents").unwrap();
        std::fs::write(dst.join("conf.txt"), b"old").unwrap();

        let (tx, rx) = async_channel::unbounded::<Event>();
        let entry = fsops::stat(Backend::Local, &src.join("conf.txt").to_string_lossy())
            .await
            .unwrap();
        transfers::start(
            TransferRequest {
                src_backend: Backend::Local,
                dst_backend: Backend::Local,
                items: vec![entry],
                dst_dir: dst.to_string_lossy().into_owned(),
                move_src: false,
                overwrite: true,
            },
            tx,
        );

        // Drain manually: a conflict prompt would hang the job (nobody
        // answers it here), so fail fast if one ever appears.
        let state = loop {
            match rx.recv().await.expect("event stream ended") {
                Event::Conflict(_) => panic!("overwrite job raised a conflict prompt"),
                Event::TransferUpdate(snap) if snap.state.is_terminal() => break snap.state,
                _ => {}
            }
        };
        assert_eq!(state, TransferState::Done);
        assert_eq!(
            std::fs::read(dst.join("conf.txt")).unwrap(),
            b"new contents"
        );
        std::fs::remove_dir_all(&base).ok();
    });
}
