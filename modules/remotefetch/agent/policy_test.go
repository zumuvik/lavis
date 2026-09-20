package main

import "testing"

func TestCheckArgsAllowsOrdinaryFastfetchOptions(t *testing.T) {
	allowed := [][]string{
		nil,
		{"--logo", "NixOS"},
		{"--logo=none"},
		{"--structure", "OS:Kernel:CPU"},
		{"--separator", " -> "},
		{"--logo-width", "20"},
		{"--logo-padding-left", "2"},
		{"--data", "inline logo data"},
		{"--pipe"},
		// Metacharacters are data: execve takes the array verbatim.
		{"--title", "a; rm -rf /"},
	}
	for _, args := range allowed {
		if err := checkArgs(args, false); err != nil {
			t.Errorf("checkArgs(%q) rejected a valid call: %v", args, err)
		}
	}
}

func TestCheckArgsRejectsFileReadingOptions(t *testing.T) {
	denied := [][]string{
		{"--file", "/etc/shadow"},
		{"--file-raw", "/home/user/.ssh/id_ed25519"},
		{"--raw", "/etc/passwd"},
		{"--config", "/tmp/evil.jsonc"},
		{"-c", "/tmp/evil.jsonc"},
		{"--gen-config", "/tmp/out.jsonc"},
		{"--logo-type", "file"},
		{"--kitty-direct", "/etc/passwd"},
		{"--iterm", "/etc/passwd"},
		{"--chafa", "/etc/passwd"},
		{"--sixel", "/etc/passwd"},
		// fastfetch matches flags case insensitively, so the policy must too.
		{"--FILE", "/etc/shadow"},
		{"--File-Raw=/etc/shadow"},
		// --logo alone still loads a path when the value looks like one.
		{"--logo", "/etc/shadow"},
		{"--logo=/etc/shadow"},
		{"-l", "~/.ssh/id_ed25519"},
		{"--logo", "./secret"},
		{"--logo", `C:\secret`},
	}
	for _, args := range denied {
		if err := checkArgs(args, false); err == nil {
			t.Errorf("checkArgs(%q) accepted a file-reading option", args)
		}
	}
}

func TestCheckArgsHonoursAllowFileArgs(t *testing.T) {
	if err := checkArgs([]string{"--file", "/etc/hostname"}, true); err != nil {
		t.Fatalf("allowFileArgs did not lift the policy: %v", err)
	}
}

func TestCheckArgsBoundsInput(t *testing.T) {
	long := make([]string, maxArgs+1)
	for i := range long {
		long[i] = "--pipe"
	}
	if err := checkArgs(long, false); err == nil {
		t.Error("checkArgs accepted more arguments than the limit")
	}
	oversized := string(make([]byte, maxArgBytes+1))
	if err := checkArgs([]string{oversized}, false); err == nil {
		t.Error("checkArgs accepted an oversized argument")
	}
	if err := checkArgs([]string{"--title\x00x"}, false); err == nil {
		t.Error("checkArgs accepted a NUL byte")
	}
	if err := checkArgs([]string{"--title\x1b[31m"}, false); err == nil {
		t.Error("checkArgs accepted an escape character")
	}
	if err := checkArgs([]string{"--title", "\xff\xfe"}, false); err == nil {
		t.Error("checkArgs accepted invalid UTF-8")
	}
}

func TestCheckArgsConsumesLogoValueAsValue(t *testing.T) {
	// The token after --logo is its value, not a flag; scanning it as a flag
	// would let "--logo --file" slip through as an unrecognized name.
	if err := checkArgs([]string{"--logo", "--file", "/etc/shadow"}, false); err == nil {
		t.Error("checkArgs accepted a file read hidden behind --logo")
	}
}

func TestNormalizeFlagIgnoresCaseAndSeparators(t *testing.T) {
	cases := map[string]string{
		"--file-raw":  "fileraw",
		"--File_Raw":  "fileraw",
		"-c":          "c",
		"--logo-type": "logotype",
		"--logo":      "logo",
	}
	for input, want := range cases {
		if got, _, _ := splitFlag(input); got != want {
			t.Errorf("splitFlag(%q) = %q, want %q", input, got, want)
		}
	}
}
