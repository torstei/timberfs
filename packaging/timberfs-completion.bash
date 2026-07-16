# bash completion for timberfs(1)
#
# `timberfs <TAB>` lists the subcommands; the positional store argument of
# the commands that take a handle or backing path (query, info, index,
# reindex, set, and the source of rotate/export) additionally offers the
# bare handles from `timberfs list --names` alongside normal file-path
# completion. When no forests are configured (or `list --names` errors),
# that call is silent and empty, so completion just falls back to files —
# no error ever reaches the terminal.
#
# Installed at /usr/share/bash-completion/completions/timberfs, where the
# bash-completion package auto-sources it for interactive shells.

_timberfs() {
    local cur prev cmd
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD - 1]}"

    local subcommands="mount create set append import export query info index list reindex rotate"

    if [ "$COMP_CWORD" -le 1 ]; then
        COMPREPLY=($(compgen -W "$subcommands" -- "$cur"))
        return 0
    fi

    cmd="${COMP_WORDS[1]}"

    # A flag that takes a value: the word after it is never a handle.
    case "$prev" in
    --into | --from | --to | --has | --any | --cutoff | --set | --unset | \
        --tail | --max | --chunk-size | --level | --flush-age | --retain | \
        --retain-size | --timestamp-regex | --timestamp-format)
        COMPREPLY=($(compgen -f -- "$cur"))
        return 0
        ;;
    esac

    case "$cur" in
    -*)
        COMPREPLY=($(compgen -f -- "$cur"))
        return 0
        ;;
    esac

    local offer_handles=0
    case "$cmd" in
    query | info | index | reindex | set)
        offer_handles=1
        ;;
    rotate | export)
        # Only the first positional (the source) is a handle; a later one
        # (rotate's DEST) is a plain name, not a store to look up.
        local positional=0 i
        for ((i = 2; i < COMP_CWORD; i++)); do
            case "${COMP_WORDS[i]}" in
            -*) ;;
            *) positional=$((positional + 1)) ;;
            esac
        done
        [ "$positional" -eq 0 ] && offer_handles=1
        ;;
    esac

    if [ "$offer_handles" -eq 1 ]; then
        local handles
        handles=$(timberfs list --names 2>/dev/null)
        COMPREPLY=($(compgen -W "$handles" -- "$cur") $(compgen -f -- "$cur"))
    else
        COMPREPLY=($(compgen -f -- "$cur"))
    fi
    return 0
}
complete -F _timberfs timberfs
