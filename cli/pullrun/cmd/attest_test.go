// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"crypto/ed25519"
	"encoding/base64"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

// TestCanonicalPayloadMatchesRuntime pins the exact bytes the runtime signs
// over. If this drifts from `pullrun_policy::canonical_payload`, every
// `pullrun sign` would be rejected by the runtime with a payload mismatch,
// so the format is asserted as a literal rather than rebuilt here.
func TestCanonicalPayloadMatchesRuntime(t *testing.T) {
	got := canonicalPayload("alpine:3.18", "abc123")
	want := "pullrun-image-v1\nalpine:3.18\nabc123\n"
	if got != want {
		t.Fatalf("canonicalPayload = %q, want %q", got, want)
	}
}

// TestSigningKeyRoundTrip exercises the keygen -> sign -> verify path that
// backs `pullrun sign`: a key written by keygen must load and produce a
// signature the corresponding public key accepts.
func TestSigningKeyRoundTrip(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "key.json")

	pub, priv, err := ed25519.GenerateKey(nil)
	if err != nil {
		t.Fatalf("generate: %v", err)
	}
	keyID := base64.RawURLEncoding.EncodeToString(pub[:6])
	file := signingKeyFile{
		KeyID:      keyID,
		Algorithm:  "ed25519",
		PrivateKey: base64.StdEncoding.EncodeToString(priv.Seed()),
		PublicKey:  base64.StdEncoding.EncodeToString(pub),
	}
	encoded, err := json.Marshal(file)
	if err != nil {
		t.Fatalf("marshal key: %v", err)
	}
	if err := os.WriteFile(path, encoded, 0o600); err != nil {
		t.Fatalf("write key: %v", err)
	}

	seed, gotID, err := loadSigningKey(path)
	if err != nil {
		t.Fatalf("loadSigningKey: %v", err)
	}
	if gotID != keyID {
		t.Fatalf("key id = %q, want %q", gotID, keyID)
	}

	payload := canonicalPayload("ghcr.io/org/app:v1", "deadbeef")
	sig := ed25519.Sign(ed25519.NewKeyFromSeed(seed), []byte(payload))
	if !ed25519.Verify(pub, []byte(payload), sig) {
		t.Fatal("signature did not verify against the generated public key")
	}
}

// TestLoadSigningKeyRejectsBadInput covers the failure modes a user will hit
// when hand-writing or truncating a key file.
func TestLoadSigningKeyRejectsBadInput(t *testing.T) {
	dir := t.TempDir()

	write := func(name, body string) string {
		p := filepath.Join(dir, name)
		if err := os.WriteFile(p, []byte(body), 0o600); err != nil {
			t.Fatalf("write %s: %v", name, err)
		}
		return p
	}

	t.Run("short private key", func(t *testing.T) {
		p := write("short.json", `{"key_id":"k","algorithm":"ed25519","private_key":"AAAA"}`)
		if _, _, err := loadSigningKey(p); err == nil {
			t.Fatal("expected an error for a short private key")
		}
	})

	t.Run("missing key id", func(t *testing.T) {
		seed := make([]byte, ed25519.SeedSize)
		p := write("noid.json", `{"algorithm":"ed25519","private_key":"`+
			base64.StdEncoding.EncodeToString(seed)+`"}`)
		if _, _, err := loadSigningKey(p); err == nil {
			t.Fatal("expected an error for a missing key_id")
		}
	})

	t.Run("unsupported algorithm", func(t *testing.T) {
		seed := make([]byte, ed25519.SeedSize)
		p := write("rsa.json", `{"key_id":"k","algorithm":"rsa","private_key":"`+
			base64.StdEncoding.EncodeToString(seed)+`"}`)
		if _, _, err := loadSigningKey(p); err == nil {
			t.Fatal("expected an error for a non-ed25519 algorithm")
		}
	})
}
