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

_timberfs_follower_names() {
    local -a names
    names=(${(f)"$(timberfs follower list --names 2>/dev/null)"})
    _describe -t followers 'follower' names
}

_timberfs_follower_verbs() {
    local -a verbs
    verbs=(
        'create:register a follower: a selection, and a consumer to feed it'
        'list:list every registered follower'
        'status:show one follower'\''s declaration and its position per store'
        'update:change a follower'\''s declaration'
        'delete:unregister a follower'
        'run:run a follower (what the systemd template runs)'
    )
    _describe -t commands 'timberfs follower subcommand' verbs
}

_timberfs_commands() {
    local -a subcommands
    subcommands=(
        'mount:serve a backing directory as a mounted filesystem'
        'umount:unmount a timberfs, finding the fuse helper this host has'
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
        'trim:enforce a store'\''s declared retention once, now'
        'rotate:move or drop chunks written before a cutoff'
        'feed:read a selection and hand the records to a consumer'
        'follower:manage the registered followers'
        'forward-intake:receive the Fluentd Forward protocol over TCP'
        'otlp-intake:receive OTLP/HTTP logs from OpenTelemetry senders'
    )
    _describe -t commands 'timberfs subcommand' subcommands
}

_timberfs() {
    if ((CURRENT == 2)); then
        _timberfs_commands
        return
    fi

    local cmd=${words[2]}
    if [[ $cmd == follower ]]; then
        if ((CURRENT == 3)); then
            _timberfs_follower_verbs
            return
        fi
        if [[ ${words[CURRENT-1]} == --store ]]; then
            _alternative 'handles:store handle:_timberfs_handles' 'files:file:_files'
            return
        fi
        if [[ ${words[CURRENT-1]} == --follow-from ]]; then
            _values 'where' begin end discovery
            return
        fi
        case ${words[3]} in
        status | update | delete | run) _timberfs_follower_names ;;
        esac
        return
    fi

    if [[ ${words[CURRENT-1]} == --follow-from ]]; then
        _values 'where' begin end discovery
        return
    fi

    case $cmd in
    query | info | index | reindex | set | trim | rotate | export)
        _alternative 'handles:store handle:_timberfs_handles' 'files:file:_files'
        ;;
    *)
        _files
        ;;
    esac
}

_timberfs "$@"
