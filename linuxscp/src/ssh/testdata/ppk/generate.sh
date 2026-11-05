#!/bin/sh
# Regenerates the PPK test fixtures in this directory. Needs ssh-keygen and
# puttygen (Debian/Ubuntu: putty-tools). Run from anywhere:
#
#     linuxscp/src/ssh/testdata/ppk/generate.sh
#
# The *.openssh files are the reference keys (made by ssh-keygen, no
# passphrase); every .ppk is puttygen's rendering of one of them, so the
# tests can check that decoding a PPK yields exactly the original key.
# All of these are throwaway keys that exist only for these tests.
set -eu
cd "$(dirname "$0")"

PASS_FILE="$(mktemp)"
NO_PASS_FILE="$(mktemp)"
trap 'rm -f "$PASS_FILE" "$NO_PASS_FILE"' EXIT
# Keep in sync with PASSPHRASE in src/ssh/ppk.rs tests.
printf 'linuxscp-test-passphrase\n' > "$PASS_FILE"
# An empty line: puttygen reads the passphrase from a file's first line, and
# generating a fresh key prompts for one unless told otherwise.
printf '\n' > "$NO_PASS_FILE"

rm -f ./*.openssh ./*.openssh.pub ./*.ppk

gen() { # name type [bits]
    ssh-keygen -q -t "$2" ${3:+-b "$3"} -N '' -C "linuxscp test $1" -f "$1.openssh"
}
gen ed25519 ed25519
gen rsa rsa 3072
gen ecdsa256 ecdsa 256
gen ecdsa384 ecdsa 384
gen ecdsa521 ecdsa 521

for t in ed25519 rsa ecdsa256 ecdsa384 ecdsa521; do
    puttygen "$t.openssh" -O private -o "$t-v3-plain.ppk"
    puttygen "$t.openssh" -O private -o "$t-v3-aes.ppk" --new-passphrase "$PASS_FILE"
    puttygen "$t.openssh" -O private -o "$t-v2-aes.ppk" --new-passphrase "$PASS_FILE" \
        --ppk-param version=2
done
puttygen ed25519.openssh -O private -o ed25519-v2-plain.ppk --ppk-param version=2
# The other Argon2 flavours, with small parameters so the tests stay quick.
puttygen ed25519.openssh -O private -o ed25519-v3-argon2i.ppk --new-passphrase "$PASS_FILE" \
    --ppk-param kdf=argon2i,memory=512,passes=2
puttygen ed25519.openssh -O private -o ed25519-v3-argon2d.ppk --new-passphrase "$PASS_FILE" \
    --ppk-param kdf=argon2d,memory=512,passes=2
# Key types the importer must refuse.
puttygen -q -t dsa -b 2048 -C 'linuxscp test dsa' -o dsa-v2-plain.ppk --ppk-param version=2 \
    --new-passphrase "$NO_PASS_FILE"
puttygen -q -t ed448 -C 'linuxscp test ed448' -o ed448-v3-plain.ppk --new-passphrase "$NO_PASS_FILE"

ls -1 ./*.ppk | wc -l
