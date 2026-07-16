#compdef timber-filter
#
# zsh completion for timber-filter(1). Installed at
# /usr/share/zsh/vendor-completions/_timber-filter, a directory zsh's
# vendor completion system adds to fpath by default, so it's autoloaded
# with no per-user setup.

_timber_filter_handles() {
    local -a handles
    handles=(${(f)"$(timberfs list --names 2>/dev/null)"})
    _describe -t handles 'store handle' handles
}

_timber_filter() {
    _arguments -s \
        '--has=[word-anchored phrase the entry must contain]:text:' \
        '--has-caseless=[as --has, compared caselessly]:text:' \
        '--substring=[literal the entry must contain, even inside longer words]:text:' \
        '--substring-caseless=[as --substring, compared caselessly]:text:' \
        '--regex=[regular expression the entry must match]:pattern:' \
        '--any=[word-anchored phrase; at least one --any must match]:text:' \
        '--any-caseless=[as --any, compared caselessly]:text:' \
        '--not-has=[word-anchored phrase the entry must NOT contain]:text:' \
        '--not-has-caseless=[as --not-has, compared caselessly]:text:' \
        '--not-substring=[literal the entry must NOT contain anywhere]:text:' \
        '--not-substring-caseless=[as --not-substring, compared caselessly]:text:' \
        '--not-regex=[regular expression the entry must NOT match]:pattern:' \
        '--from=[start of the time window]:time:' \
        '--to=[end of the time window]:time:' \
        {-c,--count}'[print only the number of matching entries]' \
        '--max=[stop after at most N matching entries]:n:' \
        {-0,--null}'[NUL-terminated entry records]' \
        '--no-filename[never prefix output lines with the source name]' \
        '--records[typed record stream out, for the next timber-aware stage]' \
        '--quiet[suppress informational notes on stderr]' \
        '--timestamp-regex=[custom entry-boundary timestamp regex]:regex:' \
        '--timestamp-format=[chrono format string for the captured timestamp]:format:' \
        {-h,--help}'[print help]' \
        {-V,--version}'[print version]' \
        '*:store or file:_alternative "handles:store handle:_timber_filter_handles" "files:file:_files"'
}

_timber_filter "$@"
