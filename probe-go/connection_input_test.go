package main

import (
	"strings"
	"testing"
)

func TestResolveConnectionURLLiteralDoesNotLookupEnvironment(t *testing.T) {
	const literal = "sqlite:fixture.db"
	called := false
	got, err := resolveConnectionURL(literal, "", func(string) (string, bool) {
		called = true
		return "", false
	})
	if err != nil {
		t.Fatal(err)
	}
	if got != literal {
		t.Fatalf("resolved URL = %q, want %q", got, literal)
	}
	if called {
		t.Fatal("literal URL unexpectedly looked up the environment")
	}
}

func TestResolveConnectionURLEnvironmentSelection(t *testing.T) {
	const (
		name  = "SG_URL"
		value = "sqlserver://fixture@localhost/demo"
	)
	lookedUp := ""
	got, err := resolveConnectionURL("", name, func(gotName string) (string, bool) {
		lookedUp = gotName
		return value, true
	})
	if err != nil {
		t.Fatal(err)
	}
	if got != value || lookedUp != name {
		t.Fatalf("resolved URL = %q from %q, want %q from %q", got, lookedUp, value, name)
	}
}

func TestResolveConnectionURLRejectsInvalidInputCombinations(t *testing.T) {
	for _, test := range []struct {
		name    string
		rawURL  string
		envName string
		want    string
	}{
		{name: "both", rawURL: "sqlite:literal.db", envName: "SG_URL", want: "only one"},
		{name: "neither", want: "one of --url or --url-env"},
	} {
		t.Run(test.name, func(t *testing.T) {
			_, err := resolveConnectionURL(test.rawURL, test.envName, func(string) (string, bool) {
				t.Fatal("invalid input unexpectedly looked up the environment")
				return "", false
			})
			if err == nil || !strings.Contains(err.Error(), test.want) {
				t.Fatalf("error = %v, want substring %q", err, test.want)
			}
		})
	}
}

func TestResolveConnectionURLRejectsAbsentOrEmptyEnvironment(t *testing.T) {
	for _, test := range []struct {
		name  string
		value string
		set   bool
		want  string
	}{
		{name: "absent", want: "not set"},
		{name: "empty", set: true, want: "empty"},
	} {
		t.Run(test.name, func(t *testing.T) {
			_, err := resolveConnectionURL("", "SG_URL", func(string) (string, bool) {
				return test.value, test.set
			})
			if err == nil || !strings.Contains(err.Error(), test.want) {
				t.Fatalf("error = %v, want substring %q", err, test.want)
			}
		})
	}
}

func TestResolveConnectionURLErrorsDoNotDiscloseInput(t *testing.T) {
	secretURL := "postgres://fixture:super-secret@localhost/catalog"
	_, err := resolveConnectionURL(secretURL, "SG_SECRET_URL", func(string) (string, bool) {
		return "", false
	})
	if err == nil || strings.Contains(err.Error(), secretURL) || strings.Contains(err.Error(), "super-secret") {
		t.Fatalf("error disclosed connection input: %v", err)
	}
}

func TestEnvironmentURLParseErrorsDoNotDiscloseCredentials(t *testing.T) {
	value := "postgres://fixture:super-secret%xx@localhost/catalog"
	resolved, err := resolveConnectionURL("", "SG_URL", func(string) (string, bool) {
		return value, true
	})
	if err != nil {
		t.Fatal(err)
	}
	_, err = parseConnection(resolved)
	if err == nil || strings.Contains(err.Error(), "super-secret") {
		t.Fatalf("parse error disclosed environment URL credentials: %v", err)
	}
}
