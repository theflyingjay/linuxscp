# PPK test fixtures

Throwaway keys for the PuTTY `.ppk` importer's tests (`src/ssh/ppk.rs`,
`src/ssh/key_import.rs`, `tests/ppk_import.rs`). They were generated for
these tests and are not used anywhere else; treat them as public.

- `*.openssh` / `*.openssh.pub` — reference keys made by `ssh-keygen`, no
  passphrase.
- `<key>-v3-plain.ppk`, `<key>-v3-aes.ppk`, `<key>-v2-aes.ppk` — puttygen's
  rendering of each reference key: PPK version 3 (PuTTY 0.75+) unencrypted
  and encrypted, and version 2 (PuTTY 0.52–0.74) encrypted.
- `ed25519-v2-plain.ppk` — a v2 file with no encryption (its MAC is keyed on
  the empty passphrase).
- `ed25519-v3-argon2i.ppk`, `ed25519-v3-argon2d.ppk` — the other Argon2
  flavours, with small parameters.
- `dsa-v2-plain.ppk`, `ed448-v3-plain.ppk` — key types the importer refuses.

Every encrypted file uses the passphrase `linuxscp-test-passphrase`.
`generate.sh` recreates the whole set (needs `ssh-keygen` and `puttygen`
from `putty-tools`).
