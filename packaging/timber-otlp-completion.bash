# bash completion for timber-otlp(1)
#
# timber-otlp has no subcommands, only flags and one positional: the
# store to ship. It offers the bare handles from `timberfs list --names`
# alongside normal file-path completion — same as the timberfs and
# timber-filter scripts. When no forests are configured (or `list
# --names` errors), that call is silent and empty, so completion just
# falls back to files.
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
--dry-run -f --follow --cursor --start --from --to --quiet \
-h --help -V --version"

    # A flag that takes a value: the word after it is never a handle.
    case "$prev" in
    --start)
        COMPREPLY=($(compgen -W "end begin" -- "$cur"))
        return 0
        ;;
    --encoding)
        COMPREPLY=($(compgen -W "proto json" -- "$cur"))
        return 0
        ;;
    --compress)
        COMPREPLY=($(compgen -W "none gzip" -- "$cur"))
        return 0
        ;;
    --cursor)
        COMPREPLY=($(compgen -f -- "$cur"))
        return 0
        ;;
    --endpoint | --header | --timeout | --service | --resource | \
        --severity-regex | --batch-size | --batch-timeout | --from | --to)
        return 0
        ;;
    esac

    case "$cur" in
    -*)
        COMPREPLY=($(compgen -W "$flags" -- "$cur"))
        return 0
        ;;
    esac

    local handles
    handles=$(timberfs list --names 2>/dev/null)
    COMPREPLY=($(compgen -W "$handles" -- "$cur") $(compgen -f -- "$cur"))
    return 0
}
complete -F _timber_otlp timber-otlp
