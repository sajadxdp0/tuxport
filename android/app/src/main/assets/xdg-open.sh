#!/bin/sh
# Tuxport's launcher reads the current bridge endpoint on every invocation, including in tmux.
exec /usr/local/libexec/tuxport-open-url "$@"
