#compdef timberfs
#
# zsh completion for timberfs(1). Installed at
# /usr/share/zsh/vendor-completions/_timberfs, a directory zsh's vendor
# completion system adds to fpath by default, so it's autoloaded with no
# per-user setup.

_timberfs_handles() {
    local -a handles
    handles=(${(f)"$(timberfs list --names 2>/dev/null)"})
    _describe -t handles 'store handle' handles
}

_timberfs_commands() {
    local -a subcommands
    subcommands=(
        'mount:serve a backing directory as a mounted filesystem'
        'create:create an empty log with declared properties'
        'set:change a store manifest'
        'append:append stdin to a log, no mount needed'
        'import:import plain log files into a store'
        'export:export a time window into a new store or bundle'
        'query:print entries written in a time window'
        'info:show a store'\''s vital signs'
        'index:show a store'\''s write-time chunk index'
        'list:list every store across the configured forests'
        'reindex:rebuild a store'\''s token index'
        'rotate:move or drop chunks written before a cutoff'
    )
    _describe -t commands 'timberfs subcommand' subcommands
}

_timberfs() {
    if ((CURRENT == 2)); then
        _timberfs_commands
        return
    fi

    local cmd=${words[2]}
    case $cmd in
    query | info | index | reindex | set | rotate | export)
        _alternative 'handles:store handle:_timberfs_handles' 'files:file:_files'
        ;;
    *)
        _files
        ;;
    esac
}

_timberfs "$@"
