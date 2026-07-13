#!/usr/bin/env bash
# Spin up an isolated local sshd for testing LinuxSCP without touching the
# system ssh setup. Creates a throwaway keypair, ssh_config alias "testbox",
# and a fake `su` (password: secret123) so the elevation flow can be tested.
#
#   scripts/test-server.sh start   # start sshd on 127.0.0.1:2299
#   scripts/test-server.sh stop
#   scripts/test-server.sh run     # start, then run the app against it
#
# Then run the integration tests:
#   cargo test -p linuxscp --test sftp_roundtrip -- --ignored
set -euo pipefail

TD="${LINUXSCP_TEST_DIR:-/tmp/linuxscp-test}"
PORT=2299
SFTP_SERVER=$(for c in /usr/lib/openssh/sftp-server /usr/libexec/openssh/sftp-server \
    /usr/lib/ssh/sftp-server; do [ -x "$c" ] && echo "$c" && break; done)

start() {
    stop 2>/dev/null || true
    mkdir -p "$TD/sshd" "$TD/home/.ssh" "$TD/bin"
    [ -f "$TD/sshd/hostkey" ] || ssh-keygen -q -t ed25519 -N '' -f "$TD/sshd/hostkey"
    [ -f "$TD/home/.ssh/id_ed25519" ] || ssh-keygen -q -t ed25519 -N '' -f "$TD/home/.ssh/id_ed25519"
    cp "$TD/home/.ssh/id_ed25519.pub" "$TD/sshd/authorized_keys"
    chmod 700 "$TD/home/.ssh"; chmod 600 "$TD/sshd/authorized_keys"

    cat > "$TD/bin/su" <<'EOS'
#!/bin/sh
CMD=; while [ $# -gt 0 ]; do case "$1" in -c) CMD="$2"; shift 2;; *) shift;; esac; done
stty -echo < /dev/tty; printf 'Password: ' > /dev/tty
IFS= read -r pw < /dev/tty; stty echo < /dev/tty; printf '\n' > /dev/tty
[ "$pw" = "secret123" ] || { echo "su: Authentication failure" >&2; exit 1; }
exec /bin/sh -c "$CMD"
EOS
    chmod +x "$TD/bin/su"

    cat > "$TD/sshd/sshd_config" <<EOF
Port $PORT
ListenAddress 127.0.0.1
HostKey $TD/sshd/hostkey
AuthorizedKeysFile $TD/sshd/authorized_keys
PasswordAuthentication no
UsePAM no
StrictModes no
Subsystem sftp $SFTP_SERVER
PidFile $TD/sshd/sshd.pid
SetEnv PATH=$TD/bin:/usr/local/bin:/usr/bin:/bin
EOF

    cat > "$TD/home/.ssh/config" <<EOF
Host testbox
  HostName 127.0.0.1
  Port $PORT
  User $(id -un)
  IdentityFile $TD/home/.ssh/id_ed25519
  UserKnownHostsFile $TD/home/.ssh/known_hosts
  StrictHostKeyChecking accept-new
EOF
    chmod 600 "$TD/home/.ssh/config"

    /usr/sbin/sshd -f "$TD/sshd/sshd_config" -E "$TD/sshd/log"
    echo "sshd listening on 127.0.0.1:$PORT (alias: testbox, dir: $TD)"
}

stop() {
    [ -f "$TD/sshd/sshd.pid" ] && kill "$(cat "$TD/sshd/sshd.pid")" 2>/dev/null || true
    echo "stopped"
}

case "${1:-start}" in
    start) start ;;
    stop) stop ;;
    run)
        start
        LINUXSCP_SSH_ARGS="-F $TD/home/.ssh/config" cargo run -p linuxscp
        ;;
    *) echo "usage: $0 {start|stop|run}"; exit 1 ;;
esac
