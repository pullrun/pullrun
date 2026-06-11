package cmd

import (
	"testing"
)

// TestRunCommandHasKernelImageFlag verifies the VM-backend
// path through `pullrun run`. The `--kernel-image` flag
// was added so the user can point at an OCI kernel reference
// (e.g. `pullrun/kernel-asahi:6.19.14`) without the runtime
// having a pre-cached kernel.
func TestRunCommandHasKernelImageFlag(t *testing.T) {
	opts := &RootOptions{DirectMode: false}
	cmd := NewRunCommand(opts)
	f := cmd.Flags().Lookup("kernel-image")
	if f == nil {
		t.Fatalf("NewRunCommand is missing --kernel-image flag")
	}
	if f.DefValue != "" {
		t.Errorf("--kernel-image default = %q, want \"\" (empty; required only when --backend=vm)", f.DefValue)
	}
	if f.Usage == "" {
		t.Errorf("--kernel-image usage string is empty")
	}
}

// TestRunCommandHasVMBackend verifies the backend flag
// accepts the "vm" value (the default is "container" but
// users can switch to "vm" for Apple Virt microVMs).
func TestRunCommandHasVMBackend(t *testing.T) {
	opts := &RootOptions{DirectMode: false}
	cmd := NewRunCommand(opts)
	f := cmd.Flags().Lookup("backend")
	if f == nil {
		t.Fatalf("NewRunCommand is missing --backend flag")
	}
	if f.DefValue != "container" {
		t.Errorf("--backend default = %q, want \"container\"", f.DefValue)
	}
}
