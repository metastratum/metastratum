// Package client talks to a metastratum controller's API. The types and
// calls are generated from the controller's OpenAPI document; this file adds
// authentication, errors and waiting for tasks.
package client

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strings"
	"time"
)

// API is a client for one controller, authenticated with an API token.
type API struct {
	*ClientWithResponses
}

// New connects to the controller at endpoint (e.g. https://mst.example.com)
// with an API token.
func New(endpoint, token, userAgent string) (*API, error) {
	c, err := NewClientWithResponses(strings.TrimRight(endpoint, "/"),
		WithRequestEditorFn(func(_ context.Context, req *http.Request) error {
			req.Header.Set("Authorization", "Bearer "+token)
			req.Header.Set("User-Agent", userAgent)
			return nil
		}))
	if err != nil {
		return nil, err
	}
	return &API{c}, nil
}

// Error is an answer the API gave instead of what was asked for.
type Error struct {
	Status  int
	Code    string `json:"error"`
	Message string `json:"message"`
}

func (e *Error) Error() string {
	if e.Message == "" {
		return fmt.Sprintf("HTTP %d", e.Status)
	}
	return fmt.Sprintf("%s (HTTP %d)", e.Message, e.Status)
}

// IsNotFound reports whether err is the API saying the thing does not exist.
func IsNotFound(err error) bool {
	e, ok := err.(*Error)
	return ok && e.Status == http.StatusNotFound
}

// IsConflict says whether err is the API refusing a change in the
// resource's current state (409).
func IsConflict(err error) bool {
	e, ok := err.(*Error)
	return ok && e.Status == http.StatusConflict
}

// Check turns an unexpected status into an *Error with the API's message.
func Check(status int, body []byte, want ...int) error {
	for _, w := range want {
		if status == w {
			return nil
		}
	}
	e := &Error{Status: status}
	_ = json.Unmarshal(body, e)
	return e
}

// Wait polls a task until it ends; an error says why it failed.
func (a *API) Wait(ctx context.Context, task TaskId) error {
	tick := time.NewTicker(time.Second)
	defer tick.Stop()
	for {
		r, err := a.GetTaskWithResponse(ctx, task)
		if err != nil {
			return err
		}
		if err := Check(r.StatusCode(), r.Body, http.StatusOK); err != nil {
			return err
		}
		switch r.JSON200.State {
		case "succeeded":
			return nil
		case "failed", "cancelled":
			msg := "no reason given"
			if r.JSON200.Error != nil {
				msg = *r.JSON200.Error
			}
			return fmt.Errorf("task %s %s: %s", r.JSON200.Kind, r.JSON200.State, msg)
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-tick.C:
		}
	}
}
