package main

import (
	"reflect"
	"testing"
)

func TestTokenizeGroupsQuotedArguments(t *testing.T) {
	cases := []struct {
		input string
		want  []string
	}{
		{"", nil},
		{"   ", nil},
		{"--logo NixOS", []string{"--logo", "NixOS"}},
		{`--separator " -> "`, []string{"--separator", " -> "}},
		{"--separator ' | '", []string{"--separator", " | "}},
		{`--title "a\"b"`, []string{"--title", `a"b`}},
		{`--title a\ b`, []string{"--title", "a b"}},
		{`--logo ""`, []string{"--logo", ""}},
		{"--structure OS:Kernel:CPU", []string{"--structure", "OS:Kernel:CPU"}},
		// No shell runs, so metacharacters must survive as literal data.
		{"--title 'a; rm -rf /'", []string{"--title", "a; rm -rf /"}},
		{"--title $(id)", []string{"--title", "$(id)"}},
	}
	for _, tc := range cases {
		got, err := tokenize(tc.input)
		if err != nil {
			t.Fatalf("tokenize(%q): unexpected error %v", tc.input, err)
		}
		if !reflect.DeepEqual(got, tc.want) {
			t.Errorf("tokenize(%q) = %#v, want %#v", tc.input, got, tc.want)
		}
	}
}

func TestTokenizeRejectsUnbalancedInput(t *testing.T) {
	for _, input := range []string{`--logo "nix`, "--logo 'nix", `--logo nix\`} {
		if _, err := tokenize(input); err == nil {
			t.Errorf("tokenize(%q) accepted unbalanced input", input)
		}
	}
}
