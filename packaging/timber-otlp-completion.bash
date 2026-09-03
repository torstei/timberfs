# bash completion for timber-otlp(1)
#
# timber-otlp is a CONSUMER: it reads a records stream on stdin and takes
# no store argument, so there are no store handles to offer here — only
# flags, and the fixed value sets of the three that have them.
#
# Installed at /usr/share/bash-completion/completions/timber-otlp,
# where the bash-completion package auto-sources it for interactive
# shells.

_timber_otlp() {
    local cur prev
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD - 1]}"

    local flags="--endpoint --header --timeout --service --resource \
--severity-regex --batch-size --batch-timeout --encoding --compress \
--dry-run --quiet -h --help -V --version"

    # A flag that takes a value: only these three have a known value set.
    case "$prev" in
    --encoding)
        COMPREPLY=($(compgen -W "proto json" -- "$cur"))
        return 0
        ;;
    --compress)
        COMPREPLY=($(compgen -W "none gzip" -- "$cur"))
        return 0
        ;;
    --endpoint | --header | --timeout | --service | --resource | \
        --severity-regex | --batch-size | --batch-timeout)
        return 0
        ;;
    esac

    COMPREPLY=($(compgen -W "$flags" -- "$cur"))
    return 0
}
complete -F _timber_otlp timber-otlp
