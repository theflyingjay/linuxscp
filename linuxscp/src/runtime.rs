use std::sync::OnceLock;

use tokio::runtime::Runtime;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// The shared tokio runtime. All SSH/SFTP work runs here; the GTK main
/// thread only awaits `JoinHandle`s and channel receivers.
pub fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("linuxscp-io")
            .build()
            .expect("failed to build tokio runtime")
    })
}
