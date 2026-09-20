package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"syscall"
	"time"
	"unicode"
)

var errUnavailable = errors.New("fastfetch not found")

const maxStderrBytes = 8 * 1024

// runFastfetch executes fastfetch with the caller's arguments. No shell is
// involved: arguments arrive as a JSON array and are passed straight to
// execve, so shell metacharacters stay data. The fixed leading flags mirror
// Lavis' own built-in command — the operator's fastfetch config is ignored so
// output stays pipe-stable and config-driven modules cannot run commands.
func runFastfetch(ctx context.Context, args []string, limit int) (string, bool, error) {
	argv := make([]string, 0, len(args)+3)
	argv = append(argv, "--config", "none", "--pipe")
	argv = append(argv, args...)

	stdout := &cappedBuffer{limit: limit}
	stderr := &cappedBuffer{limit: maxStderrBytes}

	cmd := exec.CommandContext(ctx, "fastfetch", argv...)
	cmd.Dir = "/"
	cmd.Stdin = nil
	cmd.Stdout = stdout
	cmd.Stderr = stderr
	// The session environment is what makes DE, WM, terminal and display
	// detection work, so it is inherited rather than cleared.
	cmd.Env = append(os.Environ(), "NO_COLOR=1", "CLICOLOR=0", "CLICOLOR_FORCE=0")
	// fastfetch spawns detection helpers; killing only the leader can leave
	// them running and holding the output pipe open past the deadline.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error {
		if cmd.Process == nil {
			return nil
		}
		return syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
	}
	cmd.WaitDelay = time.Second

	err := cmd.Run()
	if err != nil {
		if errors.Is(ctx.Err(), context.DeadlineExceeded) {
			return "", false, context.DeadlineExceeded
		}
		var execErr *exec.Error
		if errors.As(err, &execErr) {
			return "", false, errUnavailable
		}
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) {
			detail := sanitize(stderr.Bytes())
			if detail == "" {
				return "", false, fmt.Errorf("fastfetch exited with code %d", exitErr.ExitCode())
			}
			return "", false, fmt.Errorf("fastfetch exited with code %d: %s", exitErr.ExitCode(), firstLine(detail))
		}
		return "", false, err
	}
	return sanitize(stdout.Bytes()), stdout.truncated, nil
}

func fastfetchVersion(ctx context.Context) string {
	out, err := exec.CommandContext(ctx, "fastfetch", "--version").Output()
	if err != nil {
		return "unavailable"
	}
	return firstLine(sanitize(out))
}

func firstLine(text string) string {
	line, _, _ := strings.Cut(text, "\n")
	return line
}

// cappedBuffer keeps at most limit bytes and reports whether more arrived.
// It never returns a short write, so a bounded reader cannot make fastfetch
// die on EPIPE half way through its output.
type cappedBuffer struct {
	limit     int
	buf       []byte
	truncated bool
}

func (c *cappedBuffer) Write(p []byte) (int, error) {
	room := c.limit - len(c.buf)
	switch {
	case room <= 0:
		c.truncated = true
	case len(p) > room:
		c.buf = append(c.buf, p[:room]...)
		c.truncated = true
	default:
		c.buf = append(c.buf, p...)
	}
	return len(p), nil
}

func (c *cappedBuffer) Bytes() []byte { return c.buf }

// sanitize strips ANSI escapes and control characters. --pipe suppresses
// colors, but logo sources and module output can still carry escapes, and the
// userbot renders the result as plain Telegram text where a stray bidi
// override would let output lie about its own contents.
func sanitize(raw []byte) string {
	text := strings.ReplaceAll(string(raw), "\r\n", "\n")
	text = stripANSI(text)

	var out strings.Builder
	out.Grow(len(text))
	for _, r := range text {
		switch {
		case r == '\n' || r == '\r':
			out.WriteRune('\n')
		case r == '\t':
			out.WriteString("    ")
		case r == 0 || unicode.IsControl(r) || isBidiControl(r):
		default:
			out.WriteRune(r)
		}
	}

	lines := strings.Split(out.String(), "\n")
	for i, line := range lines {
		lines[i] = strings.TrimRight(line, " \t")
	}
	return strings.TrimRight(strings.Join(lines, "\n"), "\n")
}

// stripANSI removes escape sequences by structure rather than by regexp: a
// truncated capture can end mid-sequence, and a pattern match would then
// leave the fragment behind as visible garbage.
func stripANSI(input string) string {
	runes := []rune(input)
	var out strings.Builder
	out.Grow(len(input))
	for i := 0; i < len(runes); i++ {
		if runes[i] != 0x1b {
			out.WriteRune(runes[i])
			continue
		}
		i++
		if i >= len(runes) {
			break
		}
		switch runes[i] {
		case '[': // CSI: parameter and intermediate bytes, then a final byte.
			i++
			for i < len(runes) && (runes[i] < '@' || runes[i] > '~') {
				i++
			}
		case ']': // OSC: terminated by BEL or ST.
			i++
			for i < len(runes) {
				if runes[i] == 0x07 {
					break
				}
				if runes[i] == 0x1b && i+1 < len(runes) && runes[i+1] == '\\' {
					i++
					break
				}
				i++
			}
		case 'P', 'X', '^', '_': // DCS, SOS, PM, APC: terminated by ST.
			i++
			for i < len(runes) {
				if runes[i] == 0x1b && i+1 < len(runes) && runes[i+1] == '\\' {
					i++
					break
				}
				i++
			}
		}
	}
	return out.String()
}

func isBidiControl(r rune) bool {
	switch {
	case r == 0x061c, r == 0x200e, r == 0x200f:
		return true
	case r >= 0x202a && r <= 0x202e:
		return true
	case r >= 0x2066 && r <= 0x2069:
		return true
	default:
		return false
	}
}
