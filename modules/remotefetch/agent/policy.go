package main

import (
	"fmt"
	"strings"
	"unicode"
	"unicode/utf8"
)

const (
	maxArgs     = 64
	maxArgBytes = 256
)

// deniedFlags are the fastfetch options that read or write a path chosen by
// the caller. The agent publishes fastfetch output, not this machine's
// filesystem: without this list `--file ~/.ssh/id_ed25519` would render a
// private key as the logo. Names are normalized by normalizeFlag, so dashes
// and letter case do not matter — fastfetch itself matches flags case
// insensitively.
var deniedFlags = map[string]struct{}{
	"c":           {},
	"config":      {},
	"genconfig":   {},
	"loadconfig":  {},
	"printconfig": {},
	"file":        {},
	"fileraw":     {},
	"raw":         {},
	"sixel":       {},
	"kitty":       {},
	"kittydirect": {},
	"kittyicat":   {},
	"iterm":       {},
	"chafa":       {},
	"logotype":    {},
}

// checkArgs validates one request's fastfetch arguments. It is the only place
// the policy lives: the userbot module does not pre-check, so a compromised
// or outdated module cannot widen what this machine will run.
func checkArgs(args []string, allowFileArgs bool) error {
	if len(args) > maxArgs {
		return fmt.Errorf("too many arguments (%d, limit %d)", len(args), maxArgs)
	}
	for index := 0; index < len(args); index++ {
		arg := args[index]
		if len(arg) > maxArgBytes {
			return fmt.Errorf("argument %d is longer than %d bytes", index+1, maxArgBytes)
		}
		if !utf8.ValidString(arg) {
			return fmt.Errorf("argument %d is not valid UTF-8", index+1)
		}
		if strings.ContainsFunc(arg, unicode.IsControl) {
			return fmt.Errorf("argument %d contains control characters", index+1)
		}
		if allowFileArgs || !strings.HasPrefix(arg, "-") || arg == "-" {
			continue
		}

		name, inline, hasInline := splitFlag(arg)
		if _, denied := deniedFlags[name]; denied {
			return fmt.Errorf("option %s reads local files and is disabled; run lavis-fetchd with --allow-file-args to permit it", flagLabel(arg))
		}
		if name != "logo" && name != "l" {
			continue
		}
		// --logo without --logo-type still loads a file when the value looks
		// like one, so the value is checked even though --logo-type is denied.
		value := inline
		if !hasInline {
			if index+1 >= len(args) {
				continue
			}
			// Peeked, not consumed: the next token is rescanned as a flag on
			// the following iteration, so "--logo --file x" cannot carry a
			// denied option past this scan.
			value = args[index+1]
		}
		if looksLikePath(value) {
			return fmt.Errorf("--logo %q looks like a file path; named logos only", value)
		}
	}
	return nil
}

func splitFlag(arg string) (name string, value string, hasValue bool) {
	trimmed := strings.TrimLeft(arg, "-")
	if index := strings.IndexByte(trimmed, '='); index >= 0 {
		return normalizeFlag(trimmed[:index]), trimmed[index+1:], true
	}
	return normalizeFlag(trimmed), "", false
}

func normalizeFlag(name string) string {
	var out strings.Builder
	for _, r := range strings.ToLower(name) {
		if r == '-' || r == '_' {
			continue
		}
		out.WriteRune(r)
	}
	return out.String()
}

func flagLabel(arg string) string {
	label, _, _ := strings.Cut(arg, "=")
	return label
}

func looksLikePath(value string) bool {
	return strings.ContainsAny(value, `/\`) ||
		strings.HasPrefix(value, "~") ||
		strings.HasPrefix(value, ".")
}
