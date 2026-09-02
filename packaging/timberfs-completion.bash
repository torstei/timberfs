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

    local subcommands="mount create set append import export query info index list identity reindex trim rotate feed forest follower forward-intake otlp-intake incus-intake frames-intake frames-send"
    local follower_verbs="create list status update delete run"
    local forest_verbs="create list remove"

    if [ "$COMP_CWORD" -le 1 ]; then
        COMPREPLY=($(compgen -W "$subcommands" -- "$cur"))
        return 0
    fi

    cmd="${COMP_WORDS[1]}"

    # `forest` has its own verb layer. `create` takes a DIRECTORY — the one
    # argument in timberfs that is a path on purpose — and `remove` takes a
    # declared name.
    if [ "$cmd" = forest ]; then
        if [ "$COMP_CWORD" -le 2 ]; then
            COMPREPLY=($(compgen -W "$forest_verbs" -- "$cur"))
            return 0
        fi
        case "${COMP_WORDS[2]}" in
        create) COMPREPLY=($(compgen -d -- "$cur")) ;;
        remove)
            local fnames
            fnames=$(timberfs forest list --names 2>/dev/null)
            COMPREPLY=($(compgen -W "$fnames" -- "$cur"))
            ;;
        *) COMPREPLY=() ;;
        esac
        return 0
    fi

    # `follower` has its own verb layer, and its own names to complete.
    if [ "$cmd" = follower ]; then
        if [ "$COMP_CWORD" -le 2 ]; then
            COMPREPLY=($(compgen -W "$follower_verbs" -- "$cur"))
            return 0
        fi
        # --follow-from is three words wherever it appears.
        if [ "$prev" = --follow-from ]; then
            COMPREPLY=($(compgen -W "begin end discovery" -- "$cur"))
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

    # --follow-from takes one of three words, not a path.
    if [ "$prev" = --follow-from ]; then
        COMPREPLY=($(compgen -W "begin end discovery" -- "$cur"))
        return 0
    fi

    # --forest takes a declared forest name, and nothing else.
    if [ "$prev" = --forest ]; then
        local fnames
        fnames=$(timberfs forest list --names 2>/dev/null)
        COMPREPLY=($(compgen -W "$fnames" -- "$cur"))
        return 0
    fi

    # A flag that takes a value: the word after it is never a handle.
    # Every long flag that takes a VALUE. The word after one is never a
    # store handle, so offering handles there is wrong — file completion
    # is the useful fallback whatever the value is. Kept complete by
    # `every_value_flag_is_known_to_the_completion`, because this list
    # had silently fallen sixteen flags behind.
    case "$prev" in
    --into | --into-dir | --from | --to | --has | --any | --cutoff | --set | --unset | \
        --tail | --max | --poll | --chunk-size | --level | --flush-age | --retain | \
        --retain-size | --timestamp-regex | --timestamp-format | --listen | \
        --payload-key | --route | --max-body | --query | --select | --cursor | \
        --rotated | --socket | --project | --key | --prefix | --only | --endpoint | \
        --keep | --drain-every | --idle | --timeout | --from-chunk | --wait-for-writer | \
        --deadline | --positions | --batch-size | --follow-from)
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
