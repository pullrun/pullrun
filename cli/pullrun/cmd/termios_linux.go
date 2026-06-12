// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//go:build linux

package cmd

import "golang.org/x/sys/unix"

const termiosGetReq = unix.TCGETS
const termiosSetReq = unix.TCSETS
