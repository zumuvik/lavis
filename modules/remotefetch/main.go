// Command remotefetch is a Lavis external module (Module API v6) that reports
// fastfetch output from another machine instead of the host the userbot runs
// on. It talks to lavis-fetchd over a private network and answers the "fetch"
// command, exposed as ,remotefetch (default command) and ,remotefetch.fetch, plus ,remotefetch.status
// for agent reachability.
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"strings"
	"time"
)

const (
	protocolVersion = 6
	maxLineBytes    = 64 * 1024
	// commandBudget must terminate before the host's 5s lifecycle deadline.
	commandBudget = 4 * time.Second
	// The host guards every inbound JSON string at 8 KiB and renders module
	// text into a 4096 UTF-16 unit message with provenance appended, so the
	// reply is clipped well below both.
	maxReplyUnits = 3800
)

type request struct {
	ProtocolVersion int    `json:"protocol_version"`
	Type            string `json:"type"`
	RequestID       string `json:"request_id"`
	ModuleID        string `json:"module_id"`
	Command         string `json:"command"`
	Arguments       string `json:"arguments"`
}

type response struct {
	ProtocolVersion int     `json:"protocol_version"`
	Type            string  `json:"type"`
	RequestID       string  `json:"request_id"`
	ModuleID        string  `json:"module_id,omitempty"`
	Text            *string `json:"text,omitempty"`
	Code            string  `json:"code,omitempty"`
	Message         string  `json:"message,omitempty"`
	Actions         *[]any  `json:"actions,omitempty"`
}

func main() {
	out := bufio.NewWriter(os.Stdout)
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 4096), maxLineBytes)
	for scanner.Scan() {
		var req request
		if err := json.Unmarshal(scanner.Bytes(), &req); err != nil {
			continue
		}
		line, err := json.Marshal(handle(req))
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			continue
		}
		if _, err := out.Write(append(line, '\n')); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		if err := out.Flush(); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
	}
	if err := scanner.Err(); err != nil {
		fmt.Fprintln(os.Stderr, err)
	}
}

func handle(req request) response {
	base := response{ProtocolVersion: protocolVersion, RequestID: req.RequestID}
	if req.ProtocolVersion != protocolVersion {
		base.Type = "error"
		base.Code = "PROTOCOL_VERSION"
		base.Message = "unsupported protocol version"
		return base
	}
	switch req.Type {
	case "initialize":
		base.Type = "initialized"
		base.ModuleID = req.ModuleID
	case "health":
		base.Type = "health"
	case "shutdown":
		os.Exit(0)
	case "event":
		// The manifest declares no subscriptions; the conformance runner still
		// drives one event frame through the mandatory transcript.
		base.Type = "event_result"
		base.Actions = &[]any{}
	case "execute":
		base.Type = "result"
		text := execute(req.Command, req.Arguments)
		base.Text = &text
	default:
		base.Type = "error"
		base.Code = "UNKNOWN_TYPE"
		base.Message = "unsupported request type"
	}
	return base
}

// execute never returns a module error: a sleeping machine or a missing config
// is an expected condition and reads better as a reply than as a crash in
// `lm logs`.
func execute(command, arguments string) string {
	ctx, cancel := context.WithTimeout(context.Background(), commandBudget)
	defer cancel()

	cfg, err := loadConfig()
	if err != nil {
		return "⚠️ " + err.Error()
	}
	if command == "status" {
		return status(ctx, cfg)
	}
	return fetch(ctx, cfg, arguments)
}

func fetch(ctx context.Context, cfg *config, arguments string) string {
	args, err := tokenize(arguments)
	if err != nil {
		return "⚠️ Не разобрать аргументы: " + err.Error()
	}
	body, err := json.Marshal(fetchRequest{Args: args})
	if err != nil {
		return "⚠️ Не собрать запрос: " + err.Error()
	}

	var result fetchResponse
	if failure := call(ctx, cfg, "POST", "/v1/fastfetch", body, &result); failure != "" {
		return failure
	}
	output := strings.TrimRight(result.Output, "\n")
	if output == "" {
		return "⚠️ Fastfetch не вернул вывод."
	}

	footer := fmt.Sprintf("📍 %s · %d мс", cfg.displayName(result.Host), result.TookMS)
	if result.Truncated {
		footer += " · вывод обрезан агентом"
	}
	return clip(output) + "\n\n" + footer
}

func status(ctx context.Context, cfg *config) string {
	var result healthResponse
	started := time.Now()
	if failure := call(ctx, cfg, "GET", "/v1/health", nil, &result); failure != "" {
		return failure
	}
	return fmt.Sprintf(
		"🟢 %s на связи\n\nАдрес: %s\nFastfetch: %s\nАгент работает: %s\nОтвет за: %d мс",
		cfg.displayName(result.Host),
		cfg.URL,
		result.Fastfetch,
		humanizeDuration(time.Duration(result.UptimeS)*time.Second),
		time.Since(started).Milliseconds(),
	)
}

func humanizeDuration(d time.Duration) string {
	switch {
	case d >= 24*time.Hour:
		return fmt.Sprintf("%dд %dч", int(d.Hours())/24, int(d.Hours())%24)
	case d >= time.Hour:
		return fmt.Sprintf("%dч %dмин", int(d.Hours()), int(d.Minutes())%60)
	case d >= time.Minute:
		return fmt.Sprintf("%dмин", int(d.Minutes()))
	default:
		return fmt.Sprintf("%dс", int(d.Seconds()))
	}
}

// clip trims the reply to the host's rendering budget, counted in UTF-16
// units because that is what Telegram and the Lavis response layer measure.
func clip(text string) string {
	if utf16Len(text) <= maxReplyUnits {
		return text
	}
	const suffix = "\n… вывод обрезан"
	budget := maxReplyUnits - utf16Len(suffix)
	var out strings.Builder
	used := 0
	for _, r := range text {
		size := 1
		if r > 0xffff {
			size = 2
		}
		if used+size > budget {
			break
		}
		out.WriteRune(r)
		used += size
	}
	return out.String() + suffix
}

func utf16Len(text string) int {
	units := 0
	for _, r := range text {
		if r > 0xffff {
			units += 2
			continue
		}
		units++
	}
	return units
}
