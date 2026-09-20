package main

import (
	"fmt"
	"strings"
)

// tokenize splits a command line into fastfetch arguments the way a shell
// would group them, so `--separator " -> "` survives as one argument. No shell
// runs anywhere in this path: the tokens travel to the agent as a JSON array
// and reach execve unchanged, so metacharacters stay data.
func tokenize(input string) ([]string, error) {
	var (
		tokens  []string
		current strings.Builder
		started bool
	)
	runes := []rune(input)
	for i := 0; i < len(runes); i++ {
		r := runes[i]
		switch {
		case r == ' ' || r == '\t' || r == '\n' || r == '\r':
			if started {
				tokens = append(tokens, current.String())
				current.Reset()
				started = false
			}
		case r == '\'':
			started = true
			end := -1
			for j := i + 1; j < len(runes); j++ {
				if runes[j] == '\'' {
					end = j
					break
				}
			}
			if end < 0 {
				return nil, fmt.Errorf("незакрытая одинарная кавычка")
			}
			current.WriteString(string(runes[i+1 : end]))
			i = end
		case r == '"':
			started = true
			closed := false
			for i++; i < len(runes); i++ {
				if runes[i] == '"' {
					closed = true
					break
				}
				// Inside double quotes a backslash only escapes these.
				if runes[i] == '\\' && i+1 < len(runes) && strings.ContainsRune(`"\$`+"`", runes[i+1]) {
					i++
				}
				current.WriteRune(runes[i])
			}
			if !closed {
				return nil, fmt.Errorf("незакрытая двойная кавычка")
			}
		case r == '\\':
			started = true
			if i+1 >= len(runes) {
				return nil, fmt.Errorf("обратный слэш в конце строки")
			}
			i++
			current.WriteRune(runes[i])
		default:
			started = true
			current.WriteRune(r)
		}
	}
	if started {
		tokens = append(tokens, current.String())
	}
	return tokens, nil
}
