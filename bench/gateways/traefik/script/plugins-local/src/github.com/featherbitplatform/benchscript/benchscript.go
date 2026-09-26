// Package benchscript is the benchmark's scripting scenario for Traefik: a Yaegi-interpreted
// middleware that derives X-Bench-Script (upper-cased value + "-" + length) from X-Bench-In.
package benchscript

import (
	"context"
	"net/http"
	"strconv"
	"strings"
)

// Config is empty: the plugin takes no options.
type Config struct{}

// CreateConfig returns the (empty) plugin configuration.
func CreateConfig() *Config { return &Config{} }

// Plugin is the middleware handler.
type Plugin struct{ next http.Handler }

// New builds the middleware.
func New(ctx context.Context, next http.Handler, config *Config, name string) (http.Handler, error) {
	return &Plugin{next: next}, nil
}

func (p *Plugin) ServeHTTP(rw http.ResponseWriter, req *http.Request) {
	v := req.Header.Get("X-Bench-In")
	req.Header.Set("X-Bench-Script", strings.ToUpper(v)+"-"+strconv.Itoa(len(v)))
	p.next.ServeHTTP(rw, req)
}
