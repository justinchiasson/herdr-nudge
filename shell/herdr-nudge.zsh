# herdr-nudge zsh hook: tells Herdr when a long command in this pane
# finishes, so the herdr-nudge plugin can post a notification for it.
#
# The plugin copies this file into its state directory when Herdr starts,
# next to the shell.env it reads. Source that copy from ~/.zshrc; it does
# nothing outside a Herdr pane.
#
# A command claims the pane only once it has run for min_seconds: a
# background watcher reports `working` then, unless Herdr already has an
# agent in the pane. When it finishes, precmd sends
# the title and `idle`. Herdr turns that `idle` into `done` if nobody was
# looking at the pane, and needs the `working` first to do it. Short
# commands never call herdr at all. The claim is released by the next
# command, unless that was typed ahead, or when the shell exits.
#
# Everything runs in the user's shell, so the hook keeps its hands off
# their parameters: helpers return in REPLY, which each hook makes local,
# and nothing is started with a bare `&!`, which would change `$!`. The
# one exception is the watcher on a zsh too old to give its pid another
# way.

[[ -n ${ZSH_VERSION-} && ${HERDR_ENV-} == 1 && -n ${HERDR_PANE_ID-} ]] || return 0
zmodload zsh/datetime zsh/zselect zsh/system 2>/dev/null || return 0
zmodload -F zsh/files b:zf_rm 2>/dev/null || return 0

# Declared without values so sourcing the file twice keeps a claim we hold.
typeset -g  _herdr_nudge_bin=${HERDR_BIN_PATH:-herdr}
typeset -gi _herdr_nudge_min
typeset -gA _herdr_nudge_skip   # command names we never report
typeset -gA _herdr_nudge_agents # Herdr's agent labels, also in the skip list
typeset -g  _herdr_nudge_mark   # start of the watcher's note files' names
typeset -g  _herdr_nudge_cmd    # label of the command running now, if timed
typeset -g  _herdr_nudge_title
typeset -gF _herdr_nudge_start
typeset -gi _herdr_nudge_watcher
typeset -g  _herdr_nudge_claim  # label Herdr holds for us, empty if none
typeset -gi _herdr_nudge_last_seq
typeset -gi _herdr_nudge_ahead  # the next command was typed ahead
typeset -g  _herdr_nudge_token  # this shell's, see _herdr_nudge_same_shell

# shell.env is key=value lines written by the plugin. It is read line by
# line and never sourced, so nothing in it can run as code.
function _herdr_nudge_settings {
  emulate -L zsh
  local line key value enabled=0 min=
  [[ -r $1 ]] || return 1
  _herdr_nudge_skip=() _herdr_nudge_agents=()
  while IFS= read -r line || [[ -n $line ]]; do
    [[ $line == *=* && $line != \#* ]] || continue
    key=${line%%=*} value=${line#*=}
    case $key in
      enabled) enabled=$value ;;
      min_seconds) min=$value ;;
      ignore) [[ -n $value ]] && _herdr_nudge_skip[$value]=1 ;;
      agent) [[ -n $value ]] && _herdr_nudge_skip[$value]=1 _herdr_nudge_agents[$value]=1 ;;
    esac
  done < $1
  [[ $enabled == 1 && $min == <-> ]] || return 1
  _herdr_nudge_min=$min
}

# Missing shell.env means the plugin hasn't set this up since it was
# installed, so the agent list is missing too. Doing nothing beats claiming
# a pane that an agent CLI is about to report on itself. If an earlier
# source added the hooks (shell commands since turned off, then `source
# ~/.zshrc`), they come out too, or they'd go on with an empty skip list.
if ! _herdr_nudge_settings ${${(%):-%x}:A:h}/shell.env; then
  if (( $+functions[_herdr_nudge_zshexit] )); then
    _herdr_nudge_zshexit
    autoload -Uz add-zsh-hook
    add-zsh-hook -d preexec _herdr_nudge_preexec
    add-zsh-hook -d precmd _herdr_nudge_precmd
    add-zsh-hook -d zshexit _herdr_nudge_zshexit
  fi
  return 0
fi
_herdr_nudge_mark=${${(%):-%x}:A:h}/zsh-skip.$$
typeset -g _herdr_nudge_token_file=${${(%):-%x}:A:h}/zsh-shell.$$
# Not exported, so empty only the first time in this process, including
# after an `exec zsh` that kept the pid.
if [[ -z $_herdr_nudge_token ]]; then
  _herdr_nudge_token=$EPOCHREALTIME
  { print -r -- $_herdr_nudge_token >| $_herdr_nudge_token_file } 2>/dev/null
fi

# Herdr drops a report whose --seq isn't above the last one it took from
# this source for this pane, and it remembers that across shells and
# releases. The clock in microseconds can't go below what an earlier shell
# sent; the +1 covers two reports in the same microsecond.
function _herdr_nudge_seq {
  local -i now=$(( epochtime[1] * 1000000 + epochtime[2] / 1000 ))
  (( _herdr_nudge_last_seq = now > _herdr_nudge_last_seq ? now : _herdr_nudge_last_seq + 1 ))
  REPLY=$_herdr_nudge_last_seq
}

# Output and failures are dropped: the prompt must not care whether Herdr
# answered. stdin is closed so a background call can't stop on a tty read.
function _herdr_nudge_herdr {
  "$_herdr_nudge_bin" "$@" </dev/null &>/dev/null
}

# Sets REPLY to the program a command line runs, skipping assignments and
# wrappers like sudo. Quotes come off first, so `\vim` and `'vim'` are
# still vim. Options after a wrapper are skipped but not their values, so
# `sudo -u root make` gives `root`. An unquoted `$EDITOR`, `${EDITOR}`,
# `${EDITOR:-vim}` or `${EDITOR-vim}` is looked up, and one that comes out
# empty is skipped, as zsh skips it. Nothing else is expanded, since that
# could run code: `$(which vim)` stays as typed.
function _herdr_nudge_program {
  emulate -L zsh -o extendedglob
  local raw word
  local -a value match mbegin mend
  for raw in ${(z)1}; do
    word=${(Q)raw}
    if [[ $raw != *[\'\\]* ]] &&
       [[ $word == (#b)'$'([[:alpha:]_][[:alnum:]_]#) ||
          $word == (#b)'${'([[:alpha:]_][[:alnum:]_]#)'}' ||
          $word == (#b)'${'([[:alpha:]_][[:alnum:]_]#)(:|)-([^}]#)'}' ]]; then
      value=(${(P)match[1]})
      # The default stands in for an unset name, and with `:-` for an
      # empty one too.
      if (( $#match == 3 && ! $#value )) &&
         { [[ -n $match[2] ]] || (( ! ${+parameters[$match[1]]} )) }; then
        value=(${(Q)match[3]})
      fi
      (( $#value )) || continue
      word=$value[1]
    fi
    # Before taking the basename, which would turn `CC=/usr/bin/clang`
    # into `clang`. `=vim` is zsh's own path lookup for vim. `a=1; vim`
    # runs vim, so the separator after an assignment is skipped too.
    case $word in
      '='?*) word=${word#=} ;;
      *=*|-*|'('|'{'|'!'|';'|'&&'|'||'|'|'|'|&'|'&'|'&!'|'&|') continue ;;
    esac
    word=${word:t}
    case $word in
      sudo|env|command|builtin|noglob|nocorrect|time|nohup|nice) ;;
      *) REPLY=$word; return 0 ;;
    esac
  done
  return 1
}

# True if the line runs `exec` anywhere it could, e.g. `cd dir && exec
# zsh`. The shell is replaced, so no precmd or zshexit would ever end a
# claim. Only the start of each command counts, so `echo exec` is timed.
function _herdr_nudge_execs {
  emulate -L zsh
  local word at_start=1
  for word in ${(z)1}; do
    (( at_start )) && [[ ${(Q)word} == exec ]] && return 0
    case $word in
      ';'|'&&'|'||'|'|'|'|&'|'&'|'&!'|'&|'|'('|'{'|'!'|do|then|else) at_start=1 ;;
      *=*) ;;
      *) at_start=0 ;;
    esac
  done
  return 1
}

function _herdr_nudge_duration {
  local -i s=$1
  if (( s < 60 )); then
    REPLY=${s}s
  elif (( s < 3600 )); then
    REPLY=$(( s / 60 ))m$(( s % 60 ))s
  else
    REPLY=$(( s / 3600 ))h${(l:2::0:)$(( s % 3600 / 60 ))}m
  fi
}

# KILL, since the watcher starts out ignoring TERM as the interactive
# shell does, and a command that ends at once kills it before it gets
# past that. It has nothing to clean up.
function _herdr_nudge_stop_watcher {
  (( _herdr_nudge_watcher )) || return 0
  kill -KILL $_herdr_nudge_watcher 2>/dev/null
  _herdr_nudge_watcher=0
}

# Releases each label, then clears the title and labels we set. Herdr
# 0.9.2 releases a finished command itself once the prompt is back, so
# there the release does nothing, and the plugin can't take Herdr's own
# release for the user moving on. Clearing the title of a pane nobody
# claims makes Herdr send an `unknown` with no agent in it, on every
# version, and that is what takes the banner down there. The caller picks
# the seqs, so that a shell sending this from a background job still keeps
# its own count going up. $1 is the clear's seq, then pairs of a label and
# its seq.
function _herdr_nudge_let_go {
  local clear=$1
  shift
  while (( $# >= 2 )); do
    _herdr_nudge_herdr pane release-agent $HERDR_PANE_ID --source herdr-nudge-zsh \
      --agent $1 --seq $2
    shift 2
  done
  _herdr_nudge_herdr pane report-metadata $HERDR_PANE_ID --source herdr-nudge-zsh \
    --clear-title --clear-state-labels --seq $clear
}

# One background job, so the clear still comes after the release. The
# subshell takes the `$!` that `&!` sets, so the user's still names their
# own last job. Not a process substitution: zsh kills those when it
# exits, and a release sent from zshexit must still land.
function _herdr_nudge_release {
  [[ -n $_herdr_nudge_claim ]] || return 0
  _herdr_nudge_seq
  local -a release=($_herdr_nudge_claim $REPLY)
  _herdr_nudge_seq
  ( _herdr_nudge_let_go $REPLY $release </dev/null &>/dev/null &! )
  _herdr_nudge_claim=
}

# False once this shell has been replaced by an `exec` that loaded the
# hook again, e.g. `omz reload`, which execs zsh from inside a function
# where _herdr_nudge_execs can't see it. The pid stays the same, so the
# watcher's parent check can't tell. A new shell writes a new token to
# the file; a missing or unreadable file counts as no change.
function _herdr_nudge_same_shell {
  local now
  { read -r now < $_herdr_nudge_token_file } 2>/dev/null || return 0
  [[ $now == $_herdr_nudge_token ]]
}

# True if Herdr already has an agent in this pane. Our skip list has
# Herdr's agent labels, but people type the aliases Herdr also detects by
# (`cursor-agent`, `kiro-cli`), and Herdr labels those `cursor` and `kiro`.
# `pane get` doesn't say who reported a label, so a label that isn't an
# agent's (`make` from another shell) is taken over as before. No answer
# within 2 s counts as no agent: a stopped server never answers, and an
# extra banner is better than a missed one.
function _herdr_nudge_agent_here {
  emulate -L zsh
  local out chunk fd label
  local -F deadline=$(( EPOCHREALTIME + 2 ))
  exec {fd}< <("$_herdr_nudge_bin" pane get $HERDR_PANE_ID </dev/null 2>/dev/null)
  while (( EPOCHREALTIME < deadline )) &&
      sysread -t $(( deadline - EPOCHREALTIME )) -i $fd chunk; do
    out+=$chunk
  done
  exec {fd}<&-
  # A quote inside a JSON string is escaped, so this only matches a key.
  # Herdr's keys come sorted, so the pane's `agent` comes before the one
  # inside `agent_session`, which is taken only when the pane has none.
  [[ $out == *'"agent":"'* ]] || return 1
  label=${${out#*\"agent\":\"}%%\"*}
  (( $+_herdr_nudge_agents[$label] ))
}

# Runs in the background from preexec. It stays alive after reporting,
# until precmd kills it, so the pid precmd kills can't have been reused by
# some other process in the meantime. It leaves on its own if the shell
# goes away without a precmd, or is replaced by an exec.
function _herdr_nudge_watch {
  emulate -L zsh
  local -F deadline=$(( _herdr_nudge_start + _herdr_nudge_min ))
  local -i hundredths claimed
  # zselect can return early on a signal, so sleep until the clock says so,
  # a minute at most at a time.
  while (( EPOCHREALTIME < deadline )); do
    (( hundredths = (deadline - EPOCHREALTIME) * 100 + 1 ))
    zselect -t $(( hundredths < 6000 ? hundredths : 6000 ))
  done
  # A shell killed outright runs no zshexit, and a claim made now would
  # never be released.
  (( sysparams[ppid] == $$ )) || return 0
  if ! _herdr_nudge_same_shell; then
    _herdr_nudge_left_behind 0
    return 0
  fi
  # precmd can't see what we decide, so we leave a note for this command
  # first and take it away only when we claim. Killed while still asking,
  # we have claimed nothing, and precmd must not finish a claim either.
  local mark=$_herdr_nudge_mark.$_herdr_nudge_start
  : >| $mark
  if ! _herdr_nudge_agent_here; then
    zf_rm -f $mark && { _herdr_nudge_claim_now; claimed=1 }
  fi
  # First a second, then the gap doubles up to ten, so a late exec is
  # still noticed within ten seconds without waking every second for a
  # command that runs for hours.
  local -i gap=100
  while (( sysparams[ppid] == $$ )); do
    zselect -t $gap
    (( gap = gap < 500 ? gap * 2 : 1000 ))
    _herdr_nudge_same_shell && continue
    _herdr_nudge_left_behind $claimed
    return 0
  done
}

# In the watcher, once an exec has replaced the shell. The new shell knows
# nothing of this command, or of a claim the old one still held because
# this command was typed ahead, so nobody else would release them. $1 is
# whether the watcher claimed the pane for this command.
function _herdr_nudge_left_behind {
  local label
  local -a labels
  zf_rm -f $_herdr_nudge_mark.$_herdr_nudge_start 2>/dev/null
  (( $1 )) && labels+=($_herdr_nudge_cmd)
  [[ -n $_herdr_nudge_claim ]] && labels+=($_herdr_nudge_claim)
  (( $#labels )) || return 0
  local -a releases
  for label in ${(u)labels}; do
    _herdr_nudge_seq
    releases+=($label $REPLY)
  done
  _herdr_nudge_seq
  _herdr_nudge_let_go $REPLY $releases
}

function _herdr_nudge_claim_now {
  # The last command's title is still up after a typed-ahead line, which
  # released nothing, or after Herdr's own release, so replace it before
  # the claim shows up in Herdr.
  _herdr_nudge_seq
  _herdr_nudge_herdr pane report-metadata $HERDR_PANE_ID --source herdr-nudge-zsh \
    --title $_herdr_nudge_title --clear-state-labels --seq $REPLY
  _herdr_nudge_seq
  _herdr_nudge_herdr pane report-agent $HERDR_PANE_ID --source herdr-nudge-zsh \
    --agent $_herdr_nudge_cmd --state working --seq $REPLY
}

function _herdr_nudge_preexec {
  emulate -L zsh -o extendedglob
  local REPLY
  # herdr-ohmyzsh reports this pane already, and two reporters would fight
  # over it.
  (( $+functions[_herdr_omz_preexec] )) && return 0
  _herdr_nudge_stop_watcher
  # The command before a typed-ahead one may have just finished while the
  # user was away, and releasing now would take its banner down, or stop
  # it posting.
  (( _herdr_nudge_ahead )) || _herdr_nudge_release
  _herdr_nudge_cmd=
  # $3 is the full text, with aliases expanded.
  _herdr_nudge_execs $3 && return 0
  # $1 is the line as typed, $2 with aliases expanded. Either name can be
  # on the skip list.
  _herdr_nudge_program $1 && (( ! $+_herdr_nudge_skip[$REPLY] )) || return 0
  _herdr_nudge_program $2 && (( ! $+_herdr_nudge_skip[$REPLY] )) || return 0
  _herdr_nudge_cmd=$REPLY

  local text=${${1//[[:space:]]##/ }## }
  text=${text%% }
  (( ${#text} > 60 )) && text="${text[1,59]}…"
  _herdr_nudge_title=$text

  _herdr_nudge_start=$EPOCHREALTIME
  # A process substitution leaves `$!` alone and gets its own process
  # group, as a job would. A zsh without sysparams[procsubstpid] can't
  # say its pid, so it gets `&!` and loses the user's `$!`.
  if (( $+sysparams[procsubstpid] )); then
    : <(_herdr_nudge_watch </dev/null &>/dev/null)
    _herdr_nudge_watcher=$sysparams[procsubstpid]
  else
    _herdr_nudge_watch </dev/null &>/dev/null &!
    _herdr_nudge_watcher=$!
  fi
}

function _herdr_nudge_precmd {
  local -i rc=$?
  emulate -L zsh
  local REPLY
  # Input already waiting was typed while the command ran. zle takes only
  # the line it runs, so with several lines typed ahead, each but the last
  # still sees the rest waiting. A line without its Enter doesn't count.
  _herdr_nudge_ahead=0
  zselect -t 0 -r 0 2>/dev/null && _herdr_nudge_ahead=1
  [[ -n $_herdr_nudge_cmd ]] || return 0
  local cmd=$_herdr_nudge_cmd
  _herdr_nudge_cmd=
  # Kill first, then read the clock. If the watcher reported before it
  # died, the clock is past the threshold, so we report idle too and the
  # pane isn't left stuck on working.
  _herdr_nudge_stop_watcher
  local -F elapsed=$(( EPOCHREALTIME - _herdr_nudge_start ))
  (( elapsed >= _herdr_nudge_min )) || return 0
  # The watcher's note: it found an agent in the pane, or it died before
  # it knew. Either way it made no claim, so there's none to finish.
  local mark=$_herdr_nudge_mark.$_herdr_nudge_start
  if [[ -e $mark ]]; then
    zf_rm -f $mark 2>/dev/null
    return 0
  fi

  local word=done
  (( rc )) && word=failed
  _herdr_nudge_duration $elapsed
  local title="$_herdr_nudge_title · exit $rc · $REPLY"
  _herdr_nudge_seq
  local meta_seq=$REPLY
  _herdr_nudge_seq
  local idle_seq=$REPLY
  # One background job, so the title lands before the idle it goes with.
  # In a subshell for `$!`, as in _herdr_nudge_release.
  ( {
    _herdr_nudge_herdr pane report-metadata $HERDR_PANE_ID --source herdr-nudge-zsh \
      --title $title --state-label idle=$word --seq $meta_seq
    _herdr_nudge_herdr pane report-agent $HERDR_PANE_ID --source herdr-nudge-zsh \
      --agent $cmd --state idle --seq $idle_seq
  } </dev/null &>/dev/null &! )
  _herdr_nudge_claim=$cmd
}

function _herdr_nudge_zshexit {
  emulate -L zsh
  local REPLY
  _herdr_nudge_stop_watcher
  zf_rm -f $_herdr_nudge_mark.$_herdr_nudge_start $_herdr_nudge_token_file 2>/dev/null
  _herdr_nudge_release
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec _herdr_nudge_preexec
add-zsh-hook precmd _herdr_nudge_precmd
add-zsh-hook zshexit _herdr_nudge_zshexit
