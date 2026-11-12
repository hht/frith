# Frith shell integration for zsh (ADR 11). Marks prompts with OSC 133 so Frith clears the
# prompt when the window resizes and zsh redraws it, instead of the old prompt staying on
# screen. zsh reads this file because Frith points ZDOTDIR here; it puts the user's ZDOTDIR
# back and runs the user's .zshenv, so zsh goes on to read the user's own startup files.
#
# Marks: A starts a prompt, B ends it (input follows), C starts a command's output, D ends
# the command with its exit status.
# @see https://gitlab.freedesktop.org/Per_Bothner/specifications/blob/master/proposals/semantic-prompts.md
# @see https://sw.kovidgoyal.net/kitty/shell-integration/#notes-for-shell-developers

if [[ -n ${FRITH_ZSH_ZDOTDIR+set} ]]; then
  ZDOTDIR=$FRITH_ZSH_ZDOTDIR
  unset FRITH_ZSH_ZDOTDIR
else
  unset ZDOTDIR
fi

if [[ -o interactive ]]; then
  # Tell Frith now, once, that zsh redraws its whole prompt (redraw=1): nothing has been
  # printed yet, so the cursor is at the start of a line and the mark's fresh-line does
  # nothing. Later marks without a redraw option, such as p10k's, leave this setting alone.
  # The C after it makes what the startup files print output, which a resize never clears,
  # even if no prompt is ever marked.
  builtin print -n '\e]133;A;redraw=1\a\e]133;C\a'
fi

[[ ! -r ${ZDOTDIR-$HOME}/.zshenv ]] || builtin source -- "${ZDOTDIR-$HOME}/.zshenv"

if [[ -o interactive ]]; then
  # p10k rebuilds its prompt in a precmd hook it keeps last, so marks another hook adds to
  # PS1 are gone before the prompt is drawn. This global, which p10k itself sets for kitty
  # and Warp, makes p10k mark the prompt it builds (and send C and D as well).
  # @see https://github.com/romkatv/powerlevel10k/blob/604f19a9eaa18e76db2e60b8d446d5f879065f90/internal/p10k.zsh#L7433-L7436
  (( ${+__p9k_force_term_shell_integration} )) || typeset -gi __p9k_force_term_shell_integration=1

  typeset -gi _frith_running=0

  _frith_precmd() {
    local exit_status=$?
    if (( _frith_running )); then
      builtin print -n "\e]133;D;$exit_status\a"
      _frith_running=0
    fi
    # Mark a prompt nobody else has marked, where zsh expands %{ %} rather than showing it.
    if [[ -o prompt_percent && $PS1 != *$'\e]133;A'* ]]; then
      PS1=$'%{\e]133;A;redraw=1\a%}'$PS1$'%{\e]133;B\a%}'
    fi
  }

  _frith_preexec() {
    _frith_running=1
    builtin print -n '\e]133;C\a'
  }

  builtin autoload -Uz add-zsh-hook
  add-zsh-hook precmd _frith_precmd
  add-zsh-hook preexec _frith_preexec
fi
