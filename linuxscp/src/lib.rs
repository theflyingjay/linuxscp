//! LinuxSCP core: SSH transport, SFTP operations and the transfer engine.
//! The GTK UI lives in the binary; everything here is UI-free and testable.

pub mod fsops;
pub mod runtime;
pub mod secrets;
pub mod sessions;
pub mod settings;
pub mod ssh;
pub mod transfers;
pub mod types;
