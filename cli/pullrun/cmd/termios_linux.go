//go:build linux

package cmd

import "golang.org/x/sys/unix"

const termiosGetReq = unix.TCGETS
const termiosSetReq = unix.TCSETS
