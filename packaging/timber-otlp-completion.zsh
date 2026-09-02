#compdef timber-otlp
#
# zsh completion for timber-otlp(1). Installed at
# /usr/share/zsh/vendor-completions/_timber-otlp, a directory zsh's
# vendor completion system adds to fpath by default, so it's autoloaded
# with no per-user setup.
#
# It is a CONSUMER, reading a records stream on stdin, so there is no
# store argument to complete.

_timber_otlp() {
    _arguments -s \
        '--endpoint=[OTLP/HTTP receiver: base URL or the signal URL]:url:' \
        '*--header=[extra request header]:k=v:' \
        '--timeout=[connect/read/write timeout per request]:duration:' \
        '--service=[resource service.name; overrides every store own]:name:' \
        '*--resource=[extra resource attribute]:k=v:' \
        '--severity-regex=[where the level is, if not an uppercase level word]:pattern:' \
        '--batch-size=[maximum LogRecords per export request]:n:' \
        '--batch-timeout=[send a partial batch after this long with nothing new]:duration:' \
        '--encoding=[wire encoding]:encoding:(proto json)' \
        '--compress=[compress request bodies]:mode:(none gzip)' \
        '--dry-run[print the export requests instead of sending them]' \
        '--quiet[suppress progress notes on stderr]' \
        {-h,--help}'[print help]' \
        {-V,--version}'[print version]'
}

_timber_otlp "$@"
