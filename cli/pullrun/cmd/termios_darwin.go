//go:build darwin

package cmd

import "golang.org/x/sys/unix"

const termiosGetReq = unix.TIOCGETA
const termiosSetReq = unix.TIOCSETA
