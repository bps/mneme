# Shell completion

`mn` ships dynamic shell completion via `clap_complete`'s runtime engine. The
binary is the single source of truth: subcommand names, aliases, flags, and
*live session names* (for `attach`, `auto`, `kill`) are all generated on demand
by invoking `mn` itself.

## Install

Pick the snippet for your shell. Each registers a small stub that calls back
into `mn` on `<Tab>`; you do not need to regenerate anything when `mn` is
upgraded or new subcommands are added.

### bash

```bash
# ~/.bashrc
source <(COMPLETE=bash mn)
```

### zsh

```zsh
# ~/.zshrc — needs to run after `compinit`
source <(COMPLETE=zsh mn)
```

Or, for a more conventional install, drop the script into your `$fpath`:

```zsh
COMPLETE=zsh mn > ~/.zfunc/_mn   # ~/.zfunc must be in $fpath before compinit
```

### fish

```fish
# ~/.config/fish/completions/mn.fish
COMPLETE=fish mn | source
```

### elvish

```elvish
# ~/.config/elvish/rc.elv
eval (COMPLETE=elvish mn | slurp)
```

### powershell

```powershell
# $PROFILE
COMPLETE=powershell mn | Out-String | Invoke-Expression
```

## What gets completed

| Position                    | Candidates                                              |
|-----------------------------|---------------------------------------------------------|
| top-level                   | `create`, `new`, `attach`, `auto`, `list`, `kill`, aliases |
| `mn attach <TAB>`           | live session names                                      |
| `mn auto <TAB>`             | live session names                                      |
| `mn kill <TAB>`             | live session names + `all`                              |
| `mn <subcmd> -<TAB>`        | flags, with help text                                   |

Stale sessions (server crashed, lock released) are filtered out so you cannot
tab-complete to a name that no longer works.
