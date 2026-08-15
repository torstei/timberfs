#compdef timber-otlp
#
# zsh completion for timber-otlp(1). Installed at
# /usr/share/zsh/vendor-completions/_timber-otlp, a directory zsh's
# vendor completion system adds to fpath by default, so it's autoloaded
# with no per-user setup.

_timber_otlp_handles() {
    local -a handles
    handles=(${(f)"$(timberfs list --names 2>/dev/null)"})
    _describe -t handles 'store handle' handles
}

_timber_otlp() {
    _arguments -s \
        '--endpoint=[OTLP/HTTP receiver: base URL or the signal URL]:url:' \
        '*--header=[extra request header]:k=v:' \
        '--timeout=[connect/read/write timeout per request]:duration:' \
        '--service=[resource service.name]:name:' \
        '*--resource=[extra resource attribute]:k=v:' \
        '--severity-regex=[where the level is, if not an uppercase level word]:pattern:' \
        '--batch-size=[maximum LogRecords per export request]:n:' \
        '--batch-timeout=[send a partial batch after this long with nothing new]:duration:' \
        '--dry-run[print the export requests instead of sending them]' \
        {-f,--follow}'[keep shipping as entries are committed]' \
        '--cursor=[persist the shipping position here]:file:_files' \
        '--start=[where to start with no cursor file yet]:where:(end begin)' \
        '--from=[replay: start of the logline window]:time:' \
        '--to=[replay: end of the logline window]:time:' \
        '--quiet[suppress progress notes on stderr]' \
        {-h,--help}'[print help]' \
        {-V,--version}'[print version]' \
        '1:store:_alternative "handles:store handle:_timber_otlp_handles" "files:file:_files"'
}

_timber_otlp "$@"
