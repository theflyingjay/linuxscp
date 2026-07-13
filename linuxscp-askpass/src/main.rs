//! SSH_ASKPASS helper for LinuxSCP.
//!
//! OpenSSH invokes this binary with the prompt as argv[1] whenever it needs
//! a password, key passphrase, or host-key confirmation. We forward the
//! prompt to the LinuxSCP GUI over the unix socket named by
//! `LINUXSCP_ASKPASS_SOCK` and relay the user's answer back.
//!
//! Wire format (all lengths are u32 big-endian):
//!   request:  len ++ prompt-utf8
//!   response: status-byte ++ len ++ answer-utf8
//!     status 0 = answer follows (password text)
//!     status 1 = confirmed (print "yes", exit 0)
//!     status 2 = cancelled/denied (exit 1)

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::exit;

fn run() -> Result<u8, Box<dyn std::error::Error>> {
    let prompt = std::env::args().nth(1).unwrap_or_default();
    let sock_path = std::env::var("LINUXSCP_ASKPASS_SOCK")?;
    let mut stream = UnixStream::connect(sock_path)?;

    let bytes = prompt.as_bytes();
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(bytes)?;
    stream.flush()?;

    let mut status = [0u8; 1];
    stream.read_exact(&mut status)?;
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let mut answer = vec![0u8; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut answer)?;

    match status[0] {
        0 => {
            let mut out = std::io::stdout();
            out.write_all(&answer)?;
            out.write_all(b"\n")?;
            out.flush()?;
            Ok(0)
        }
        1 => {
            println!("yes");
            Ok(0)
        }
        _ => Ok(1),
    }
}

fn main() {
    match run() {
        Ok(code) => exit(code as i32),
        Err(err) => {
            eprintln!("linuxscp-askpass: {err}");
            exit(1);
        }
    }
}
