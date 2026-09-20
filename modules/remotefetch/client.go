package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"time"
)

// httpTimeout leaves room inside commandBudget for config loading and for the
// reply to be written before the host's lifecycle deadline expires.
const httpTimeout = 3500 * time.Millisecond

// maxResponseBytes bounds what the agent may return. The agent caps fastfetch
// output itself; this guards against a hostile or broken endpoint.
const maxResponseBytes = 256 * 1024

type fetchRequest struct {
	Args []string `json:"args"`
}

type fetchResponse struct {
	Output    string `json:"output"`
	Host      string `json:"host"`
	TookMS    int64  `json:"took_ms"`
	Truncated bool   `json:"truncated"`
}

type healthResponse struct {
	OK        bool   `json:"ok"`
	Host      string `json:"host"`
	Fastfetch string `json:"fastfetch"`
	UptimeS   int64  `json:"uptime_s"`
}

type agentError struct {
	Error string `json:"error"`
}

var client = &http.Client{Timeout: httpTimeout}

// call performs one agent request and decodes its result. It returns an empty
// string on success, or a ready-to-send user-facing message describing the
// failure.
func call(ctx context.Context, cfg *config, method, path string, body []byte, out any) string {
	var reader io.Reader
	if body != nil {
		reader = bytes.NewReader(body)
	}
	request, err := http.NewRequestWithContext(ctx, method, cfg.URL+path, reader)
	if err != nil {
		return "⚠️ Некорректный адрес агента: " + cfg.URL
	}
	request.Header.Set("Authorization", "Bearer "+cfg.Token)
	request.Header.Set("Accept", "application/json")
	if body != nil {
		request.Header.Set("Content-Type", "application/json")
	}

	response, err := client.Do(request)
	if err != nil {
		if errors.Is(err, context.DeadlineExceeded) || isTimeout(err) {
			return fmt.Sprintf("⏱ %s не ответил за %s.", cfg.displayName(""), httpTimeout)
		}
		return fmt.Sprintf(
			"🔌 %s недоступен (%s). Проверь, что он не спит, в сети и lavis-fetchd запущен.",
			cfg.displayName(""), cfg.URL,
		)
	}
	defer response.Body.Close()

	payload, err := io.ReadAll(io.LimitReader(response.Body, maxResponseBytes))
	if err != nil {
		return "⚠️ Оборвался ответ агента."
	}
	if response.StatusCode != http.StatusOK {
		return agentFailure(response.StatusCode, payload)
	}
	if err := json.Unmarshal(payload, out); err != nil {
		return "⚠️ Непонятный ответ агента."
	}
	return ""
}

func agentFailure(status int, payload []byte) string {
	var failure agentError
	detail := ""
	if err := json.Unmarshal(payload, &failure); err == nil {
		detail = failure.Error
	}
	switch status {
	case http.StatusUnauthorized:
		return "🔒 Агент отклонил токен. Сверь token в конфиге модуля и --token-file агента."
	case http.StatusTooManyRequests:
		return "⏳ Агент занят другим запросом, повтори."
	case http.StatusGatewayTimeout:
		return "⏱ Fastfetch на удалённой машине не уложился в таймаут агента."
	case http.StatusServiceUnavailable:
		return "⚠️ На удалённой машине нет fastfetch."
	}
	if detail == "" {
		return fmt.Sprintf("⚠️ Агент вернул HTTP %d.", status)
	}
	return "⚠️ " + clip(detail)
}

func isTimeout(err error) bool {
	var timeout interface{ Timeout() bool }
	return errors.As(err, &timeout) && timeout.Timeout()
}
