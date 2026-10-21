# SPDX-License-Identifier: MIT
# Copyright (c) 2003-2026 Eelco Dolstra and the Nixpkgs/NixOS contributors
# Nixpkgs auditTmpdir, scoped to the opt-in packaging candidate.
# Adapted from proposed Nixpkgs fix (not yet merged):
# https://github.com/NixOS/nixpkgs/pull/575856
# commit bed6939a16c497e68079bb5bd67e712919394412,
# pkgs/build-support/setup-hooks/audit-tmpdir.sh; notice: nixpkgs-LICENSE.
# Define only the function: keep the standard fixupOutputHooks registration.

auditTmpdir() {
    local dir="$1"
    [ -e "$dir" ] || return 0
    echo "checking for references to $TMPDIR/ in $dir..."
    local tmpdir elf_fifo script_fifo
    tmpdir="$(mktemp -d)" || return $?
    elf_fifo="$tmpdir/elf"
    script_fifo="$tmpdir/script"
    local setup_status=0
    mkfifo "$elf_fifo" "$script_fifo" || setup_status=$?
    if (( setup_status != 0 )); then
        rm -r "$tmpdir"
        return "$setup_status"
    fi
    (
        # Byte classification does not need user locale. Keep this change in
        # the forked classifier: restoring a default locale can enter Darwin's
        # CoreFoundation/gettext path after fork. This is a scoped mitigation,
        # not a claim that every Bash/macOS locale crash has been fixed.
        export LC_ALL=C
        set -o pipefail
        if find "$dir" -type f -not -path '*/.build-id/*' -print0 |
        while IFS= read -r -d $'\0' file; do
            if isELF "$file"; then
                printf '%s\0' "$file" >&3 || exit $?
            elif isScript "$file"; then
                filename=${file##*/}
                dir=${file%/*}
                if [ -e "$dir/.$filename-wrapped" ]; then
                    printf '%s\0' "$file" >&4 || exit $?
                fi
            fi
        done; then
            pipeline_status=0
        else
            pipeline_status=$?
        fi
        exec 3>&- 4>&-
        exit "$pipeline_status"
    ) 3> "$elf_fifo" 4> "$script_fifo" &
    local pid_classifier=$!
    (
        xargs -0 -r -P "$NIX_BUILD_CORES" -n 1 sh -c '
            if { printf :; patchelf --print-rpath "$1"; } | grep -q -F ":$TMPDIR/"; then
                echo "RPATH of binary $1 contains a forbidden reference to $TMPDIR/"
                exit 1
            fi
        ' _ < "$elf_fifo"
    ) &
    local pid_elf=$!
    (
        xargs -0 -r -P "$NIX_BUILD_CORES" -n 1 sh -c '
            if grep -q -F "$TMPDIR/" "$1"; then
                echo "wrapper script $1 contains a forbidden reference to $TMPDIR/"
                exit 1
            fi
        ' _ < "$script_fifo"
    ) &
    local pid_script=$!
    local classifier_status=0 elf_status=0 script_status=0 cleanup_status=0
    if wait "$pid_classifier"; then
        classifier_status=0
    else
        classifier_status=$?
        echo "auditTmpdir: classifier/find pipeline failed with status $classifier_status" >&2
    fi
    if wait "$pid_elf"; then
        elf_status=0
    else
        elf_status=$?
        echo "auditTmpdir: ELF handler failed with status $elf_status" >&2
    fi
    if wait "$pid_script"; then
        script_status=0
    else
        script_status=$?
        echo "auditTmpdir: script handler failed with status $script_status" >&2
    fi
    if rm -r "$tmpdir"; then
        cleanup_status=0
    else
        cleanup_status=$?
        echo "auditTmpdir: could not remove temporary directory (status $cleanup_status)" >&2
    fi
    if (( classifier_status != 0 )); then
        return "$classifier_status"
    elif (( elf_status != 0 )); then
        return "$elf_status"
    elif (( script_status != 0 )); then
        return "$script_status"
    else
        return "$cleanup_status"
    fi
}
