package protocol

import (
	"strings"
	"testing"
)

func TestSanitizeBotAgent(t *testing.T) {
	cases := []struct {
		name string
		in   string
		want string
	}{
		{"empty", "", DefaultBotAgent},
		{"whitespace only", "   ", DefaultBotAgent},
		{"single product", "MyApp/1.2", "MyApp/1.2"},
		{"product with comment", "MyApp/1.2 (prod build)", "MyApp/1.2 (prod build)"},
		{"multiple products", "MyApp/1.2 (prod) Lib/0.3", "MyApp/1.2 (prod) Lib/0.3"},
		{"normalizes whitespace", "  MyApp/1.2   Lib/0.3 ", "MyApp/1.2 Lib/0.3"},
		{"no slash", "no-slash", DefaultBotAgent},
		{"invalid token", "bad name/1.0 !!!", DefaultBotAgent},
		{"orphan comment", "(orphan comment)", DefaultBotAgent},
		{"unclosed comment", "App/1.0 (unclosed", DefaultBotAgent},
		{"nested comment", "App/1.0 (nested (comment))", DefaultBotAgent},
		{"name too long", strings.Repeat("a", 33) + "/1.0", DefaultBotAgent},
		{"version too long", "App/" + strings.Repeat("1", 33), DefaultBotAgent},
		{"over byte cap", strings.TrimSpace(strings.Repeat("App/1.0 ", 40)), DefaultBotAgent},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := SanitizeBotAgent(tc.in); got != tc.want {
				t.Errorf("SanitizeBotAgent(%q) = %q, want %q", tc.in, got, tc.want)
			}
		})
	}
}
