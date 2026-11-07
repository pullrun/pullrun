// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

package cmd

import (
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"time"

	"github.com/spf13/cobra"
)

// This file provides the ingestion side of the policy engine: the commands
// that put a signature or an SBOM into the DAG store.
//
// Without these, a runtime started with --require-signature / --require-sbom
// could only ever deny, because nothing in the product could produce the
// attestations the gate checks for.
//
// The signature is ed25519 over a canonical payload owned by the runtime:
//
//	pullrun-image-v1\n<image_ref>\n<manifest_digest>\n
//
// The runtime re-derives that payload from the request and rejects a
// signature that does not verify against it, so this side only ever supplies
// signature bytes — never a payload the runtime would trust.

// signingKeyFile is the on-disk format written by `pullrun keygen`.
//
// The private key is a raw ed25519 seed (32 bytes) because that is what
// ed25519.NewKeyFromSeed expects; it is stored base64 so the file stays JSON
// and diffable, and created 0600.
type signingKeyFile struct {
	KeyID      string `json:"key_id"`
	Algorithm  string `json:"algorithm"`
	PrivateKey string `json:"private_key"`
	PublicKey  string `json:"public_key"`
}

// canonicalPayload builds the exact payload the runtime signs over and
// verifies against. It must stay in lockstep with
// `pullrun_policy::canonical_payload`; the runtime rejects any mismatch, so a
// drift here surfaces as a clear error rather than a silent false negative.
func canonicalPayload(imageRef, manifestDigest string) string {
	return fmt.Sprintf("pullrun-image-v1\n%s\n%s\n", imageRef, manifestDigest)
}

// NewKeygenCommand generates an ed25519 keypair for signing images.
func NewKeygenCommand() *cobra.Command {
	var outPath string

	cmd := &cobra.Command{
		Use:   "keygen",
		Short: "Generate an ed25519 keypair for signing images",
		Long: `Generate an ed25519 keypair for signing images.

The private key is written to a 0600 file and is what "pullrun sign" reads.
The public key is printed in the "<id>:<base64>" form the runtime accepts via
--trusted-key.`,
		RunE: func(cmd *cobra.Command, args []string) error {
			pub, priv, err := ed25519.GenerateKey(rand.Reader)
			if err != nil {
				return fmt.Errorf("generate key: %w", err)
			}
			seed := priv.Seed()
			keyID := base64.RawURLEncoding.EncodeToString(pub[:6])

			file := signingKeyFile{
				KeyID:      keyID,
				Algorithm:  "ed25519",
				PrivateKey: base64.StdEncoding.EncodeToString(seed),
				PublicKey:  base64.StdEncoding.EncodeToString(pub),
			}
			data, err := json.MarshalIndent(file, "", "  ")
			if err != nil {
				return fmt.Errorf("encode key: %w", err)
			}
			data = append(data, '\n')

			if outPath == "" {
				outPath = "pullrun-signing-key.json"
			}
			// 0600 from creation: the file holds a private key.
			if err := os.WriteFile(outPath, data, 0o600); err != nil {
				return fmt.Errorf("write %s: %w", outPath, err)
			}

			fmt.Printf("✓ keypair written to %s (mode 0600)\n\n", outPath)
			fmt.Printf("  key id:     %s\n", keyID)
			fmt.Printf("  trusted key: %s:%s\n\n",
				keyID, base64.StdEncoding.EncodeToString(pub))
			fmt.Println("Give the trusted key to the runtime:")
			fmt.Printf("  pullrun-runtime daemon --trusted-key %s:<base64-public-key>\n",
				keyID)
			fmt.Println("\nThen sign an image with:")
			fmt.Printf("  pullrun sign <image-ref> --manifest <digest> --key %s\n", outPath)
			return nil
		},
	}
	cmd.Flags().StringVarP(&outPath, "out", "o", "", "path to write the private key (default pullrun-signing-key.json)")
	return cmd
}

// NewSignCommand signs an image manifest and records the signature.
func NewSignCommand(opts *RootOptions) *cobra.Command {
	var (
		manifestDigest string
		keyPath        string
		printPayload   bool
	)

	cmd := &cobra.Command{
		Use:   "sign [IMAGE_REF]",
		Short: "Sign an image manifest and record the signature",
		Long: `Sign an image manifest with an ed25519 key and store the signature.

The image reference must be the same string you use with "pullrun pull" or
"pullrun run": the signature is bound to that exact reference, and a policy
re-check at run time looks it up by the recorded reference.

Example:
  pullrun pull alpine:3.18
  pullrun sign alpine:3.18 --manifest <digest> --key pullrun-signing-key.json`,
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			imageRef := args[0]
			if manifestDigest == "" {
				return fmt.Errorf("--manifest is required")
			}
			if keyPath == "" {
				return fmt.Errorf("--key is required")
			}

			payload := canonicalPayload(imageRef, manifestDigest)
			if printPayload {
				fmt.Print(payload)
				return nil
			}

			seed, keyID, err := loadSigningKey(keyPath)
			if err != nil {
				return err
			}
			priv := ed25519.NewKeyFromSeed(seed)
			signature := ed25519.Sign(priv, []byte(payload))

			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			resp, err := client.AttachSignature(ctx, imageRef, manifestDigest, keyID, signature)
			if err != nil {
				return fmt.Errorf("attach signature: %w", err)
			}

			fmt.Printf("✓ signed %s\n", imageRef)
			fmt.Printf("  key:              %s\n", keyID)
			fmt.Printf("  signature digest: %s\n", resp.SignatureDigest)
			if !resp.Verified {
				return fmt.Errorf("runtime reported the signature as unverified")
			}
			return nil
		},
	}

	cmd.Flags().StringVar(&manifestDigest, "manifest", "", "manifest digest of the image (hex, optional sha256: prefix)")
	cmd.Flags().StringVar(&keyPath, "key", "", "path to a key file from `pullrun keygen`")
	cmd.Flags().BoolVar(&printPayload, "print-payload", false, "print the canonical payload and exit (debugging)")
	return cmd
}

func loadSigningKey(path string) (ed25519.PrivateKey /*seed*/, string, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, "", fmt.Errorf("read key %s: %w", path, err)
	}
	var f signingKeyFile
	if err := json.Unmarshal(raw, &f); err != nil {
		return nil, "", fmt.Errorf("parse key %s: %w", path, err)
	}
	if f.Algorithm != "" && f.Algorithm != "ed25519" {
		return nil, "", fmt.Errorf("key %s: unsupported algorithm %q", path, f.Algorithm)
	}
	seed, err := base64.StdEncoding.DecodeString(f.PrivateKey)
	if err != nil {
		return nil, "", fmt.Errorf("key %s: decode private key: %w", path, err)
	}
	if len(seed) != ed25519.SeedSize {
		return nil, "", fmt.Errorf("key %s: private key must be %d bytes, got %d",
			path, ed25519.SeedSize, len(seed))
	}
	if f.KeyID == "" {
		return nil, "", fmt.Errorf("key %s: missing key_id", path)
	}
	return seed, f.KeyID, nil
}

// NewSbomCommand groups SBOM tooling.
func NewSbomCommand(opts *RootOptions) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "sbom",
		Short: "Manage SBOMs used by the policy engine",
	}
	cmd.AddCommand(NewSbomAttachCommand(opts))
	return cmd
}

// NewSbomAttachCommand attaches a CycloneDX SBOM to an image manifest.
func NewSbomAttachCommand(opts *RootOptions) *cobra.Command {
	var (
		manifestDigest string
		file           string
	)

	cmd := &cobra.Command{
		Use:   "attach",
		Short: "Attach a CycloneDX JSON SBOM to an image manifest",
		Long: `Attach a CycloneDX 1.5 JSON SBOM to an image manifest.

A policy with --require-sbom (or a CVSS cap / license deny-list) reads this
SBOM at pull and run time. Example:

  pullrun sbom attach --manifest <digest> --file sbom.json`,
		RunE: func(cmd *cobra.Command, args []string) error {
			if manifestDigest == "" {
				return fmt.Errorf("--manifest is required")
			}
			if file == "" {
				return fmt.Errorf("--file is required")
			}
			data, err := os.ReadFile(file)
			if err != nil {
				return fmt.Errorf("read %s: %w", file, err)
			}

			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			resp, err := client.AttachSbom(ctx, manifestDigest, string(data))
			if err != nil {
				return fmt.Errorf("attach sbom: %w", err)
			}

			fmt.Printf("✓ SBOM attached to %s\n", manifestDigest)
			fmt.Printf("  components:      %d\n", resp.Components)
			fmt.Printf("  vulnerabilities: %d\n", resp.Vulnerabilities)
			fmt.Printf("  sbom digest:     %s\n", resp.SbomDigest)
			return nil
		},
	}

	cmd.Flags().StringVar(&manifestDigest, "manifest", "", "manifest digest of the image (hex, optional sha256: prefix)")
	cmd.Flags().StringVarP(&file, "file", "f", "", "CycloneDX JSON file")
	return cmd
}

// NewAttestCommand reports the attestations recorded for an image.
func NewAttestCommand(opts *RootOptions) *cobra.Command {
	var manifestDigest string

	cmd := &cobra.Command{
		Use:   "attest [IMAGE_REF]",
		Short: "Show the signature and SBOM recorded for an image",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			if manifestDigest == "" {
				return fmt.Errorf("--manifest is required")
			}
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			client, closeFn, err := ensureGRPCClient(opts)
			if err != nil {
				return err
			}
			defer closeFn()

			resp, err := client.GetAttestations(ctx, args[0], manifestDigest)
			if err != nil {
				return fmt.Errorf("get attestations: %w", err)
			}

			if resp.HasSignature {
				fmt.Printf("signature: present (key %s)\n", resp.KeyId)
			} else {
				fmt.Println("signature: none")
			}
			if resp.HasSbom {
				fmt.Printf("sbom:      present (%d components, %d vulnerabilities",
					resp.Components, resp.Vulnerabilities)
				if resp.MaxCvss > 0 {
					fmt.Printf(", max CVSS %.1f", resp.MaxCvss)
				}
				fmt.Println(")")
				if len(resp.Licenses) > 0 {
					fmt.Printf("licenses:  %v\n", resp.Licenses)
				}
			} else {
				fmt.Println("sbom:      none")
			}
			return nil
		},
	}

	cmd.Flags().StringVar(&manifestDigest, "manifest", "", "manifest digest of the image (hex, optional sha256: prefix)")
	return cmd
}
