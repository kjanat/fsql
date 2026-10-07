#!/usr/bin/env bash
# Cargo passes the compiled executable followed by its arguments.
set -euo pipefail

host_os=$(uname -s)
if [[ ${host_os} != Linux ]]; then
	echo 'fsql sandbox: Linux is required' >&2
	exit 1
fi
if ! command -v bwrap >/dev/null; then
	echo 'fsql sandbox: install bubblewrap (bwrap) first' >&2
	exit 127
fi
if (($# == 0)); then
	echo 'usage: sandbox-runner.sh EXECUTABLE [ARGUMENTS...]' >&2
	exit 2
fi

program=$(realpath -e -- "$1")
shift
if [[ ! -f ${program} || ! -x ${program} ]]; then
	echo 'fsql sandbox: expected an executable file' >&2
	exit 2
fi

args=(
	--unshare-all --unshare-user --disable-userns
	--die-with-parent --new-session --cap-drop ALL
	--ro-bind / /
	--proc /proc --dev /dev
	--tmpfs /tmp --tmpfs /run
	--dir /tmp/fsql-sandbox --dir /tmp/fsql-home
	--ro-bind "${program}" /tmp/fsql-program
	--setenv HOME /tmp/fsql-home
	--setenv XDG_DATA_HOME /tmp/fsql-home/.local/share
	--setenv XDG_CONFIG_HOME /tmp/fsql-home/.config
	--setenv XDG_CACHE_HOME /tmp/fsql-home/.cache
	--setenv XDG_STATE_HOME /tmp/fsql-home/.local/state
	--setenv XDG_RUNTIME_DIR /tmp/fsql-home/run
	--setenv TMPDIR /tmp
	--chdir /tmp/fsql-sandbox
)

if [[ -n ${FSQL_SANDBOX_SEED+x} ]]; then
	seed=$(realpath -e -- "${FSQL_SANDBOX_SEED}")
	if [[ ! -d ${seed} ]]; then
		echo 'fsql sandbox: FSQL_SANDBOX_SEED must name a directory' >&2
		exit 2
	fi
	args+=(--ro-bind "${seed}" /tmp/fsql-seed)
fi

# Copy inside the namespace: no writable host scratch directory or cleanup trap.
# Preserve symlinks instead of following them into the host filesystem.
exec bwrap "${args[@]}" -- /bin/sh -eu -c '
    if [ -d /tmp/fsql-seed ]; then
        cp -a -- /tmp/fsql-seed/. /tmp/fsql-sandbox/
    fi
    exec "$@"
' sandbox /tmp/fsql-program "$@"
