package main

import (
	"embed"
	"net/http"
	"path"
)

//go:embed web
var web embed.FS

// page serves one embedded file with headers that keep the static pages inert.
func page(name string) http.HandlerFunc {
	b, err := web.ReadFile("web/" + name)
	if err != nil {
		panic(err) // a missing embedded page is a build mistake
	}
	ctype := "text/html; charset=utf-8"
	if path.Ext(name) == ".css" {
		ctype = "text/css; charset=utf-8"
	}
	return func(w http.ResponseWriter, _ *http.Request) {
		h := w.Header()
		h.Set("Content-Type", ctype)
		h.Set("Content-Security-Policy", "default-src 'none'; style-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'")
		h.Set("X-Content-Type-Options", "nosniff")
		h.Set("Referrer-Policy", "no-referrer")
		h.Set("Cache-Control", "no-cache") // tiny pages; always current after a deploy
		w.Write(b)
	}
}
