# bash completion for timber-filter(1)
#
# timber-filter has no subcommands, only flags and positionals. A
# positional can be a timberfs store handle, a store path, or a raw text
# file, so it offers the bare handles from `timberfs list --names`
# alongside normal file-path completion — same as the timberfs script.
# When no forests are configured (or `list --names` errors), that call
# is silent and empty, so completion just falls back to files.
#
# Installed at /usr/share/bash-completion/completions/timber-filter,
# where the bash-completion package auto-sources it for interactive
# shells.

_timber_filter() {
    local cur prev
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD - 1]}"

    local flags="--has --has-caseless --substring --substring-caseless \
--regex --any --any-caseless --not-has --not-has-caseless --not-substring \
--not-substring-caseless --not-regex --from --to -c --count --max -0 --null \
--no-filename --records --quiet --timestamp-regex --timestamp-format \
-h --help -V --version"

    # A flag that takes a value: the word after it is never a handle.
    case "$prev" in
    --has | --has-caseless | --substring | --substring-caseless | --regex | \
        --any | --any-caseless | --not-has | --not-has-caseless | \
        --not-substring | --not-substring-caseless | --not-regex | \
        --from | --to | --max | --timestamp-regex | --timestamp-format)
        COMPREPLY=($(compgen -f -- "$cur"))
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
complete -F _timber_filter timber-filter
