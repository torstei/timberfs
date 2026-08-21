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
# `timberfs follower <TAB>` lists its own verbs, and the ones that name an
# existing follower complete from `timberfs follower list --names`, which
# is silent and empty in the same way when nothing is registered.
#
# Installed at /usr/share/bash-completion/completions/timberfs, where the
# bash-completion package auto-sources it for interactive shells.

_timberfs() {
    local cur prev cmd
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD - 1]}"

    local subcommands="mount create set append import export query info index list reindex trim rotate follower forward-intake otlp-intake"
    local follower_verbs="create list status update delete run"

    if [ "$COMP_CWORD" -le 1 ]; then
        COMPREPLY=($(compgen -W "$subcommands" -- "$cur"))
        return 0
    fi

    cmd="${COMP_WORDS[1]}"

    # `follower` has its own verb layer, and its own names to complete.
    if [ "$cmd" = follower ]; then
        if [ "$COMP_CWORD" -le 2 ]; then
            COMPREPLY=($(compgen -W "$follower_verbs" -- "$cur"))
            return 0
        fi
        # --store takes a store, which is where handles belong.
        if [ "$prev" = --store ]; then
            local handles
            handles=$(timberfs list --names 2>/dev/null)
            COMPREPLY=($(compgen -W "$handles" -- "$cur") $(compgen -f -- "$cur"))
            return 0
        fi
        case "${COMP_WORDS[2]}" in
        status | update | delete | run)
            local names
            names=$(timberfs follower list --names 2>/dev/null)
            COMPREPLY=($(compgen -W "$names" -- "$cur"))
            ;;
        *) COMPREPLY=() ;;
        esac
        return 0
    fi

    # A flag that takes a value: the word after it is never a handle.
    case "$prev" in
    --into | --into-dir | --from | --to | --has | --any | --cutoff | --set | --unset | \
        --tail | --max | --poll | --chunk-size | --level | --flush-age | --retain | \
        --retain-size | --timestamp-regex | --timestamp-format | --listen | \
        --payload-key | --route | --max-body)
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
    query | info | index | reindex | set | trim)
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
